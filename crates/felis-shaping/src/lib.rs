//! `felis-shaping`: swash text shaping, fallback chain, shape cache.
//!
//! Output is a [`GlyphBitmap`] (coverage or RGBA pixels plus placement);
//! conversion to a GPU texture happens in `felis-render-wgpu`.

// Relaxes the workspace `unsafe_code = "deny"` for the single
// `Mmap::map` in `Font::load_face_id`
// (docs/explanation/rendering/text-shaping.md "Font loading").
#![allow(unsafe_code)]

use std::{collections::HashMap, sync::Arc};

use fontdb::{Database, Family, Query};
pub use swash::GlyphId;
use swash::{
    FontRef, Setting,
    scale::{Render, ScaleContext, Source, StrikeWith, image::Content},
    shape::{Direction, ShapeContext},
    zeno::Format,
};
use thiserror::Error;
use tracing::warn;

#[derive(Debug, Error)]
pub enum ShapingError {
    #[error("no monospace font installed")]
    NoFont,
    #[error("parse font face: {0}")]
    BadFont(String),
    #[error("load font {}: {detail}", path.display())]
    FontPath {
        path: std::path::PathBuf,
        detail: String,
    },
}

type FontBytes = Arc<dyn AsRef<[u8]> + Send + Sync>;

/// Single-sourced so `flake.nix`'s pinned test-font dir, the loader, and
/// every feature test agree on the face.
pub const TEST_FONT_FAMILY: &str = "Monaspace Neon";

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
    offset: u32,
    /// Generated once at load: swash keys its per-font shape and scale
    /// caches on this, so a fresh key per `font_ref()` call would rebuild
    /// the GSUB/GPOS tables every pass (21% of `shape_run` time).
    key: swash::CacheKey,
    color: bool,
}

impl Font {
    pub fn load_default() -> Result<Self, ShapingError> {
        let db = system_db();
        let (id, _) = query_default_monospace(&db)?;
        Self::load_face_id(&db, id)
    }

    pub fn try_load_with(db: &Database, family: &str) -> Result<Self, ShapingError> {
        Self::load_with(db, Family::Name(family))
    }

    /// Load the pinned OpenType-feature probe font from `$FELIS_TEST_FONT_DIR`.
    ///
    /// Not a system-font probe: probing skips silently on hosts without a
    /// ligature font. Panics if the env var is set but the family is absent.
    #[must_use]
    pub fn try_load_test_font() -> Option<Self> {
        let dir = std::path::PathBuf::from(std::env::var_os("FELIS_TEST_FONT_DIR")?);
        let mut db = Database::new();
        db.load_fonts_dir(&dir);
        let font = Self::try_load_with(&db, TEST_FONT_FAMILY).unwrap_or_else(|_| {
            panic!(
                "FELIS_TEST_FONT_DIR={} is set but {TEST_FONT_FAMILY:?} did not load",
                dir.display()
            )
        });
        Some(font)
    }

    /// Load the test font when `$FELIS_TEST_FONT_DIR` is set, else default.
    ///
    /// Benchmarks load through this to prevent host-dependent baselines and
    /// failures on font-less CI runners.
    pub fn load_test_font_or_default() -> Result<Self, ShapingError> {
        Self::try_load_test_font().map_or_else(Self::load_default, Ok)
    }

    fn load_with(db: &Database, family: Family<'_>) -> Result<Self, ShapingError> {
        let id = db
            .query(&Query {
                families: &[family],
                ..Query::default()
            })
            .ok_or(ShapingError::NoFont)?;
        Self::load_face_id(db, id)
    }

    fn load_face_id(db: &Database, id: fontdb::ID) -> Result<Self, ShapingError> {
        let face = db.face(id).ok_or(ShapingError::NoFont)?;
        let (source, face_index) = (face.source.clone(), face.index);
        let bytes: FontBytes = match source {
            fontdb::Source::Binary(bytes) | fontdb::Source::SharedFile(_, bytes) => bytes,
            // fontdb exposes a persistent mmap only behind its `unsafe`
            // `make_shared_face_data`, so map the file here: reading it
            // into a `Vec` turns a 183 MB emoji face into dirty heap
            // instead of reclaimable page cache
            // (docs/explanation/rendering/text-shaping.md "Font loading").
            fontdb::Source::File(path) => {
                let file = std::fs::File::open(&path)
                    .map_err(|e| ShapingError::BadFont(format!("open {}: {e}", path.display())))?;
                // SAFETY: `path` is a fontdb-discovered face under a
                // system font root: root-owned, read-only, not mutated
                // for the process lifetime
                // (docs/reference/security-audits.md "`O_CLOEXEC` +
                // `O_NOFOLLOW` audit (standing)"). The only documented
                // hazard of `Mmap::map` is SIGBUS if the file is
                // truncated while mapped, which does not happen for
                // these immutable OS assets. This is the same trade-off
                // fontdb's own `make_shared_face_data` and FreeType's
                // path-based `FT_New_Face` accept.
                let mmap = unsafe { memmap2::Mmap::map(&file) }
                    .map_err(|e| ShapingError::BadFont(format!("mmap {}: {e}", path.display())))?;
                Arc::new(mmap)
            }
        };
        let (offset, color) = {
            let font_ref = FontRef::from_index(bytes.as_ref().as_ref(), face_index as usize)
                .ok_or_else(|| ShapingError::BadFont("FontRef::from_index returned None".into()))?;
            let color = font_ref.color_palettes().len() != 0 || font_ref.color_strikes().len() != 0;
            (font_ref.offset, color)
        };
        Ok(Self {
            bytes,
            offset,
            key: swash::CacheKey::new(),
            color,
        })
    }

    #[must_use]
    pub fn has_glyph(&self, c: char) -> bool {
        self.font_ref().charmap().map(c) != 0
    }

    /// `None` when the face does not map `c`.
    #[must_use]
    pub fn advance_px(&self, c: char, px: f32) -> Option<f32> {
        let face = self.font_ref();
        let glyph_id = face.charmap().map(c);
        (glyph_id != 0).then(|| face.glyph_metrics(&[]).scale(px).advance_width(glyph_id))
    }

    #[must_use]
    pub const fn has_color_glyphs(&self) -> bool {
        self.color
    }

    pub(crate) fn font_ref(&self) -> FontRef<'_> {
        FontRef {
            data: self.bytes.as_ref().as_ref(),
            offset: self.offset,
            key: self.key,
        }
    }

    #[must_use]
    pub fn cell_metrics(&self, px: f32) -> CellMetrics {
        let face = self.font_ref();
        let metrics = face.metrics(&[]).scale(px);
        let glyph_id = face.charmap().map('M');
        let width = if glyph_id != 0 {
            face.glyph_metrics(&[]).scale(px).advance_width(glyph_id)
        } else {
            px * 0.6
        };
        CellMetrics::from_scaled(width, metrics.ascent, metrics.descent, metrics.leading)
    }
}

/// Rescans every installed font (~300 ms on a large system); share one
/// across probes.
fn system_db() -> Database {
    let mut db = Database::new();
    db.load_system_fonts();
    db
}

/// Tried in order when the `monospace` generic names nothing installed.
pub const MONOSPACE_FALLBACK_FAMILIES: &[&str] = &[
    "DejaVu Sans Mono",
    "Liberation Mono",
    "Noto Sans Mono",
    "Ubuntu Mono",
    "Menlo",
    "Consolas",
];

fn query_family(db: &Database, name: &str) -> Option<fontdb::ID> {
    db.query(&Query {
        families: &[Family::Name(name)],
        ..Query::default()
    })
}

/// Resolves the default face and the family its styled faces derive from.
///
/// fontdb keeps only the first `prefer` entry of the last `monospace` alias
/// it parses, so on Debian the generic names `FreeMono` (69-unifont.conf)
/// even when only `DejaVu Sans Mono` is installed.
fn query_default_monospace(db: &Database) -> Result<(fontdb::ID, String), ShapingError> {
    let generic = db.family_name(&Family::Monospace);
    if let Some(id) = query_family(db, generic) {
        return Ok((id, generic.to_owned()));
    }
    let resolve = |name: &str| query_family(db, name).map(|id| (id, name.to_owned()));
    let found = MONOSPACE_FALLBACK_FAMILIES
        .iter()
        .find_map(|name| resolve(name))
        .or_else(|| {
            db.faces()
                .filter(|face| face.monospaced)
                .filter_map(|face| face.families.first().map(|(name, _)| name.as_str()))
                .min()
                .and_then(resolve)
        });
    if let Some((_, family)) = &found {
        tracing::info!(
            generic,
            family = family.as_str(),
            "monospace generic not installed; using a fallback family"
        );
    }
    found.ok_or(ShapingError::NoFont)
}

/// A resolved `font.fallback` entry or styled font table.
///
/// `None` inherits: `family` derives from base `font.family` at the target
/// weight/slant, while `features` inherits base `font.features` (`Some([])`
/// opts out).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FaceSpec {
    pub family: Option<String>,
    pub features: Option<Vec<String>>,
}

impl FaceSpec {
    #[must_use]
    pub fn named(family: impl Into<String>) -> Self {
        Self {
            family: Some(family.into()),
            features: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct StyleFaces<'a> {
    pub bold: &'a FaceSpec,
    pub italic: &'a FaceSpec,
    pub bold_italic: &'a FaceSpec,
}

impl Default for StyleFaces<'_> {
    fn default() -> Self {
        const INHERIT: &FaceSpec = &FaceSpec {
            family: None,
            features: None,
        };
        Self {
            bold: INHERIT,
            italic: INHERIT,
            bold_italic: INHERIT,
        }
    }
}

/// Ordered chain of fonts queried per glyph (`fonts[0]` is regular primary,
/// `fonts[1..]` fallbacks). Cell metrics come from the regular primary alone,
/// so styled faces must share its advance width.
#[derive(Clone)]
pub struct FontStack {
    fonts: Vec<Arc<Font>>,
    features: Vec<Vec<String>>,
    styled_primaries: [Arc<Font>; 4],
    styled_features: [Vec<String>; 4],
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
    #[must_use]
    pub fn new(primary: Arc<Font>) -> Self {
        Self::with_primary_features(primary, Vec::new())
    }

    /// Auto-discovery over the pinned `$FELIS_TEST_FONT_DIR` set alone,
    /// so a fallback-dependent test sees the same faces on every host.
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn try_pinned_test_stack() -> Option<Self> {
        let dir = std::env::var_os("FELIS_TEST_FONT_DIR")?;
        let mut db = Database::new();
        db.load_fonts_dir(std::path::PathBuf::from(dir));
        Self::auto_discover_in(&db, None, &[], &[], &StyleFaces::default()).ok()
    }

    #[must_use]
    pub fn with_primary_features(primary: Arc<Font>, features: Vec<String>) -> Self {
        let styled_primaries = std::array::from_fn(|_| primary.clone());
        let styled_features = std::array::from_fn(|_| features.clone());
        Self {
            fonts: vec![primary],
            features: vec![features],
            styled_primaries,
            styled_features,
        }
    }

    #[must_use]
    pub fn with_fallback_features(mut self, font: Arc<Font>, features: Vec<String>) -> Self {
        self.fonts.push(font);
        self.features.push(features);
        debug_assert_eq!(self.fonts.len(), self.features.len());
        self
    }

    /// Load the primary font and fallbacks.
    ///
    /// Unknown families fall back to the default monospace face. Non-empty
    /// `explicit_fallbacks` replaces auto-discovery; `features: None` inherits
    /// `primary_features`, while `Some([])` opts out.
    pub fn auto_discover(
        primary_family: Option<&str>,
        primary_features: &[String],
        explicit_fallbacks: &[FaceSpec],
        styles: &StyleFaces<'_>,
    ) -> Result<Self, ShapingError> {
        Self::auto_discover_in(
            &system_db(),
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
        let mut db = Database::new();
        for path in files {
            let err = |detail: String| ShapingError::FontPath {
                path: path.clone(),
                detail,
            };
            let bytes = std::fs::read(path).map_err(|e| err(e.to_string()))?;
            if db
                .load_font_source(fontdb::Source::Binary(Arc::new(bytes)))
                .is_empty()
            {
                return Err(err("no font face in the file".to_owned()));
            }
        }
        Self::auto_discover_in(
            &db,
            primary_family,
            primary_features,
            explicit_fallbacks,
            styles,
        )
    }

    fn auto_discover_in(
        db: &Database,
        primary_family: Option<&str>,
        primary_features: &[String],
        explicit_fallbacks: &[FaceSpec],
        styles: &StyleFaces<'_>,
    ) -> Result<Self, ShapingError> {
        let (regular_id, base_family) = match primary_family {
            Some(name) => {
                if let Some(id) = query_family(db, name) {
                    (id, name.to_owned())
                } else {
                    warn!(
                        family = name,
                        "font family not found; falling back to monospace"
                    );
                    query_default_monospace(db)?
                }
            }
            None => query_default_monospace(db)?,
        };
        let base_family = Family::Name(&base_family);
        let regular = Arc::new(Font::load_face_id(db, regular_id)?);
        let mut stack = Self::with_primary_features(regular.clone(), primary_features.to_vec());
        stack.load_styled(
            db,
            regular_id,
            &regular,
            base_family,
            primary_features,
            styles,
        );
        if explicit_fallbacks.is_empty() {
            stack = append_first_installed(stack, db, CJK_FALLBACK_FAMILIES, primary_features);
            stack = append_all_installed(stack, db, SYMBOL_FALLBACK_FAMILIES, primary_features);
            stack = append_first_installed(stack, db, EMOJI_FALLBACK_FAMILIES, primary_features);
            stack = append_first_installed(stack, db, NERD_FONT_FAMILIES, primary_features);
        } else {
            for spec in explicit_fallbacks {
                let Some(family) = spec.family.as_deref() else {
                    warn!("font.fallback entry names no family; skipping");
                    continue;
                };
                if let Ok(font) = Font::try_load_with(db, family) {
                    let features = spec
                        .features
                        .as_deref()
                        .map_or_else(|| primary_features.to_vec(), <[String]>::to_vec);
                    stack = stack.with_fallback_features(Arc::new(font), features);
                } else {
                    warn!(family, "font.fallback entry not installed; skipping");
                }
            }
        }
        Ok(stack)
    }

    fn load_styled(
        &mut self,
        db: &Database,
        regular_id: fontdb::ID,
        regular: &Arc<Font>,
        base_family: Family<'_>,
        base_features: &[String],
        styles: &StyleFaces<'_>,
    ) {
        let mut by_id: HashMap<fontdb::ID, Arc<Font>> = HashMap::new();
        by_id.insert(regular_id, regular.clone());
        let variants = [
            (
                FontStyle {
                    bold: true,
                    italic: false,
                },
                styles.bold,
                fontdb::Weight::BOLD,
                fontdb::Style::Normal,
            ),
            (
                FontStyle {
                    bold: false,
                    italic: true,
                },
                styles.italic,
                fontdb::Weight::NORMAL,
                fontdb::Style::Italic,
            ),
            (
                FontStyle {
                    bold: true,
                    italic: true,
                },
                styles.bold_italic,
                fontdb::Weight::BOLD,
                fontdb::Style::Italic,
            ),
        ];
        for (style, over, weight, slant) in variants {
            let idx = style.index();
            let family = over.family.as_deref().map_or(base_family, Family::Name);
            self.styled_features[idx] = over
                .features
                .as_deref()
                .map_or_else(|| base_features.to_vec(), <[String]>::to_vec);
            let matched = db.query(&Query {
                families: &[family],
                weight,
                style: slant,
                ..Query::default()
            });
            self.styled_primaries[idx] = if let Some(id) = matched {
                by_id
                    .entry(id)
                    .or_insert_with(|| match Font::load_face_id(db, id) {
                        Ok(font) => Arc::new(font),
                        Err(e) => {
                            warn!(error = %e, "styled font face failed to load; using regular");
                            regular.clone()
                        }
                    })
                    .clone()
            } else {
                if over.family.is_some() {
                    warn!("styled font family not installed; using regular");
                }
                regular.clone()
            };
        }
    }

    #[must_use]
    pub fn primary(&self) -> &Arc<Font> {
        &self.fonts[0]
    }

    #[must_use]
    pub const fn styled_primary(&self, style: FontStyle) -> &Arc<Font> {
        &self.styled_primaries[style.index()]
    }

    #[must_use]
    pub fn primary_covers(&self, c: char, style: FontStyle) -> bool {
        self.styled_primaries[style.index()].has_glyph(c)
    }

    /// Falls back to the styled primary when nothing covers `c`, so the
    /// caller always has a face to rasterize (the `.notdef` box).
    #[must_use]
    pub fn resolve(&self, c: char, style: FontStyle) -> &Arc<Font> {
        let styled = &self.styled_primaries[style.index()];
        if styled.has_glyph(c) {
            return styled;
        }
        for font in &self.fonts[1..] {
            if font.has_glyph(c) {
                return font;
            }
        }
        styled
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
        if index == 0 || self.fonts.get(index).is_none() {
            return &self.styled_features[style.index()];
        }
        self.features_at(index)
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

    /// Unlike [`Self::resolve_index`], honors `style`: `0` when the
    /// styled primary covers `c`. Atlas keys must use this one, or a bold
    /// cluster whose base only the regular primary covers is shaped
    /// against a fallback yet keyed to the primary, rasterizing the
    /// wrong glyph.
    #[must_use]
    pub fn resolve_styled_index(&self, c: char, style: FontStyle) -> usize {
        if self.styled_primaries[style.index()].has_glyph(c) {
            return 0;
        }
        for (i, font) in self.fonts.iter().enumerate().skip(1) {
            if font.has_glyph(c) {
                return i;
            }
        }
        0
    }

    /// First face that covers `c` *and* carries color glyphs, `None`
    /// when none does. Routes a VS16 cluster onto the color-emoji face
    /// even when a mono symbol face earlier in the chain covers the base
    /// (U+2764 lives in Zapf Dingbats).
    #[must_use]
    pub fn resolve_color_index(&self, c: char, style: FontStyle) -> Option<usize> {
        let styled = &self.styled_primaries[style.index()];
        if styled.has_glyph(c) && styled.has_color_glyphs() {
            return Some(0);
        }
        self.fonts
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, font)| font.has_glyph(c) && font.has_color_glyphs())
            .map(|(i, _)| i)
    }

    /// Index `0` is the styled primary; an index past the chain (the
    /// [`SizingKey::with_font_id`] saturation bucket) falls back to it.
    #[must_use]
    pub fn font_at(&self, index: usize, style: FontStyle) -> &Arc<Font> {
        if index == 0 {
            return &self.styled_primaries[style.index()];
        }
        self.fonts
            .get(index)
            .unwrap_or_else(|| &self.styled_primaries[style.index()])
    }

    /// Always `false`; exists for clippy's `len_without_is_empty`.
    #[must_use]
    #[allow(clippy::unused_self)]
    pub const fn is_empty(&self) -> bool {
        false
    }
}

fn append_first_installed(
    stack: FontStack,
    db: &Database,
    candidates: &[&str],
    inherited_features: &[String],
) -> FontStack {
    for &name in candidates {
        if let Ok(font) = Font::try_load_with(db, name) {
            return stack.with_fallback_features(Arc::new(font), inherited_features.to_vec());
        }
    }
    stack
}

fn append_all_installed(
    mut stack: FontStack,
    db: &Database,
    candidates: &[&str],
    inherited_features: &[String],
) -> FontStack {
    for &name in candidates {
        if let Ok(font) = Font::try_load_with(db, name) {
            stack = stack.with_fallback_features(Arc::new(font), inherited_features.to_vec());
        }
    }
    stack
}

/// Pixel-space cell metrics at a given font size.
#[derive(Debug, Clone, Copy)]
pub struct CellMetrics {
    pub width: u32,
    pub height: u32,
    pub ascent: u32,
}

impl CellMetrics {
    /// Ascent and descent ceil separately rather than rounding their
    /// sum, which clips the top row of a full-ascent glyph
    /// (`docs/explanation/rendering/text-shaping.md` "Atlas integration").
    #[must_use]
    pub fn from_scaled(width: f32, ascent: f32, descent: f32, leading: f32) -> Self {
        let ascent = ascent.ceil();
        let height = (ascent + descent.ceil() + leading.round()).max(1.0);
        Self {
            width: width.round().max(1.0) as u32,
            height: height as u32,
            ascent: ascent as u32,
        }
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
/// Encodes OSC 66 sizing fields, [`FontStyle`] (bits 24-25), and fallback id
/// (bits 26-31). Packed into one `u32` because hashing the tuple form costs ~4×
/// on the cache hit path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SizingKey(u32);

impl SizingKey {
    /// Fields are masked to 4 bits; the VT parser already rejects OSC 66
    /// metadata past the spec ranges (`s` 1-7, `w` 0-7, `n`/`d` 0-15,
    /// `v`/`h` 0-2).
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
        let valign = (valign & 0xF) as u32;
        let halign = (halign & 0xF) as u32;
        Self(
            halign
                | (valign << 4)
                | (frac_den << 8)
                | (frac_num << 12)
                | (cell_width << 16)
                | (scale << 20),
        )
    }

    #[must_use]
    pub const fn with_style(self, style: FontStyle) -> Self {
        Self((self.0 & !(0b11 << 24)) | ((style.index() as u32) << 24))
    }

    #[must_use]
    pub const fn style(self) -> FontStyle {
        FontStyle::from_index(((self.0 >> 24) & 0b11) as usize)
    }

    /// Ids past 63 saturate into one shared bucket; a real font stack
    /// never has that many faces.
    #[must_use]
    pub const fn with_font_id(self, font_id: usize) -> Self {
        let id = if font_id > 0x3F { 0x3F } else { font_id as u32 };
        Self((self.0 & !(0x3F << 26)) | (id << 26))
    }

    #[must_use]
    pub const fn font_id(self) -> usize {
        ((self.0 >> 26) & 0x3F) as usize
    }

    /// The OSC 66 `w` field; `0` is auto.
    #[must_use]
    pub const fn cell_width(self) -> u8 {
        ((self.0 >> 16) & 0xF) as u8
    }

    #[must_use]
    pub const fn scale(self) -> u8 {
        ((self.0 >> 20) & 0xF) as u8
    }

    #[must_use]
    pub const fn frac_num(self) -> u8 {
        ((self.0 >> 12) & 0xF) as u8
    }

    #[must_use]
    pub const fn frac_den(self) -> u8 {
        ((self.0 >> 8) & 0xF) as u8
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

fn entry_cost(bitmap: &GlyphBitmap) -> usize {
    ENTRY_BYTES + bitmap.pixels.as_bytes().len()
}

/// Byte-capped glyph-bitmap cache. Eviction is arbitrary, not LRU:
/// recency is not tracked, so a hot glyph can go before a cold one.
pub struct ShapeCache {
    map: HashMap<ShapeKey, GlyphBitmap>,
    bytes_used: usize,
    cap: usize,
    /// Reused: swash keys per-font scaler state on `FontRef::key` inside
    /// the context, so a fresh `ScaleContext` per rasterize would rebuild
    /// it on every miss (reuse measured -38% on `shape_cache/miss`).
    scale_ctx: ScaleContext,
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
            scale_ctx: ScaleContext::new(),
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
            let bitmap = rasterize(&mut self.scale_ctx, font, c, px as f32);
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
    /// `felis-render-wgpu`, so a map entry would double-store them.
    /// `glyph_id == 0` returns a blank bitmap, as the char path does for
    /// a charmap miss.
    #[must_use]
    pub fn rasterize_glyph_id(&mut self, font: &Font, glyph_id: GlyphId, px: f32) -> GlyphBitmap {
        if glyph_id == 0 {
            return GlyphBitmap::blank();
        }
        rasterize_with(&mut self.scale_ctx, font, glyph_id, px)
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

fn rasterize(ctx: &mut ScaleContext, font: &Font, c: char, px: f32) -> GlyphBitmap {
    let face = font.font_ref();
    let glyph_id = face.charmap().map(c);
    if glyph_id == 0 {
        return GlyphBitmap::blank();
    }
    rasterize_with(ctx, font, glyph_id, px)
}

fn rasterize_with(ctx: &mut ScaleContext, font: &Font, glyph_id: GlyphId, px: f32) -> GlyphBitmap {
    let face = font.font_ref();
    // Unhinted on macOS: CoreText never grid-fits, so a hinted raster
    // sits beside every native window with stems snapped a pixel off.
    let hint = cfg!(not(target_os = "macos"));
    let mut scaler = ctx.builder(face).size(px).hint(hint).build();
    let image = Render::new(&[
        Source::ColorOutline(0),
        Source::ColorBitmap(StrikeWith::BestFit),
        Source::Outline,
    ])
    .format(Format::Alpha)
    .render(&mut scaler, glyph_id);
    let Some(image) = image else {
        return GlyphBitmap::blank();
    };
    let pixels = match image.content {
        Content::Mask => GlyphPixels::Coverage(image.data.clone()),
        // swash emits straight RGBA in R,G,B,A order (sbix/CBDT PNG
        // decoder and COLR blitter alike); a BGRA reorder here renders a
        // red 🦀 blue.
        Content::Color => GlyphPixels::Rgba(image.data.clone()),
        // LCD masks are RGBA-packed coverage, not color.
        Content::SubpixelMask => {
            GlyphPixels::Coverage(image.data.as_chunks::<4>().0.iter().map(|p| p[3]).collect())
        }
    };
    GlyphBitmap::new(
        image.placement.width,
        image.placement.height,
        image.placement.left,
        image.placement.top,
        pixels,
    )
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
/// a ligature spanning several cells yields one glyph with
/// `from_ligature` set, painted at its leftmost cell. Advances and
/// offsets are already in pixels at the shaper's `px`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShapedGlyph {
    pub glyph_id: GlyphId,
    pub source_byte_start: u32,
    pub source_byte_len: u32,
    pub advance_px: f32,
    pub x_offset_px: f32,
    pub y_offset_px: f32,
    pub from_ligature: bool,
}

/// A whole grapheme cluster shaped against one face (marks and joiners
/// have no standalone coverage). `font_id` is the
/// [`FontStack::resolve_styled_index`] handle the renderer packs into
/// [`SizingKey::with_font_id`].
#[derive(Debug, Clone)]
pub struct ClusterShaping {
    pub font_id: usize,
    pub glyphs: Vec<ShapedGlyph>,
}

/// Parse one `font.features` entry.
///
/// Not `Setting::parse`: swash implements literal CSS grammar requiring
/// quoted tags (`"'ss03'"`), dropping bare-tag forms like `"ss03"` or `"-clig"`.
/// Quotes are tolerated for CSS compatibility.
fn parse_feature(entry: &str) -> Option<Setting<u16>> {
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
    let value = match tokens.next() {
        None => u16::from(!negated),
        Some(_) if negated => return None,
        Some("on") => 1,
        Some("off") => 0,
        Some(v) => v.parse().ok()?,
    };
    if tokens.next().is_some() {
        return None;
    }
    Some(Setting::from((tag, value)))
}

/// Reusable swash shaper, held across frames: a fresh `ShapeContext`
/// per frame discards the per-font feature tables swash builds on first
/// use.
pub struct Shaper {
    ctx: ShapeContext,
}

impl Default for Shaper {
    fn default() -> Self {
        Self::new()
    }
}

impl Shaper {
    #[must_use]
    pub fn new() -> Self {
        Self {
            ctx: ShapeContext::new(),
        }
    }

    /// One `ShapedGlyph` per output glyph (fewer than cells when a
    /// ligature fires). Malformed `features` entries are dropped
    /// silently, as CSS does. Direction is fixed LTR: a cell row is LTR
    /// by construction and bidi reordering lives in the bidi marker
    /// render path.
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
        let parsed: Vec<Setting<u16>> = features.iter().filter_map(|s| parse_feature(s)).collect();
        let face = font.font_ref();
        let mut shaper = self
            .ctx
            .builder(face)
            .size(px)
            .direction(Direction::LeftToRight)
            .features(parsed.iter().copied())
            .build();
        shaper.add_str(text);
        let mut out = Vec::new();
        shaper.shape_with(|cluster| {
            let is_lig = cluster.is_ligature();
            let src_start = cluster.source.start;
            let src_end = cluster.source.end;
            for g in cluster.glyphs {
                out.push(ShapedGlyph {
                    glyph_id: g.id,
                    source_byte_start: src_start,
                    source_byte_len: src_end.saturating_sub(src_start),
                    advance_px: g.advance,
                    x_offset_px: g.x,
                    y_offset_px: g.y,
                    from_ligature: is_lig,
                });
            }
        });
        out
    }

    /// Shape one grapheme cluster against a single face.
    ///
    /// Resolves against the base scalar face, but anchors ZWJ sequences and
    /// VS16 selectors on the color emoji face so sequences can ligate into
    /// single color glyphs.
    pub fn shape_cluster(
        &mut self,
        stack: &FontStack,
        px: f32,
        style: FontStyle,
        text: &str,
    ) -> ClusterShaping {
        let Some(base) = text.chars().next() else {
            return ClusterShaping {
                font_id: 0,
                glyphs: Vec::new(),
            };
        };
        let font_id = if let Some(anchor) = text.chars().find(|&c| is_emoji_face_anchor(c)) {
            stack.resolve_styled_index(anchor, style)
        } else if text.contains('\u{FE0F}') {
            stack
                .resolve_color_index(base, style)
                .unwrap_or_else(|| stack.resolve_styled_index(base, style))
        } else {
            stack.resolve_styled_index(base, style)
        };
        let font = stack.font_at(font_id, style).clone();
        let features = stack.features_for(font_id, style);
        let glyphs = self.shape_run(&font, px, features, text);
        ClusterShaping { font_id, glyphs }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn font() -> Font {
        Font::load_default().expect("load font")
    }

    fn stack() -> FontStack {
        FontStack::auto_discover(None, &[], &[], &StyleFaces::default()).expect("primary must load")
    }

    #[test]
    fn loads_system_monospace_font() {
        // CI must provide a fontconfig DB with at least one monospace family.
        let font = Font::load_default().expect("load font");
        let metrics = font.cell_metrics(16.0);
        assert!(metrics.width > 0);
        assert!(metrics.height > 0);
    }

    #[test]
    fn from_scaled_ceils_ascent_and_descent_and_rounds_advance_and_leading() {
        // (case, (width, ascent, descent, leading), (width, height, ascent))
        let cases = [
            // DejaVu Sans Mono at 10 pt / 96 dpi: hhea ascender 1901, descender
            // -483, lineGap 0 at upem 2048 scales to 12.376 / 3.145 / 0, which
            // foot and kitty both size as a 17 px cell with the baseline at 13.
            (
                "fractional ascent and descent each ceil",
                (8.0, 12.376, 3.145, 0.0),
                (8, 17, 13),
            ),
            (
                "whole ascent and descent do not grow the cell",
                (8.0, 13.0, 4.0, 0.0),
                (8, 17, 13),
            ),
            (
                "leading below the half pixel rounds down",
                (8.0, 12.0, 4.0, 2.4),
                (8, 18, 12),
            ),
            (
                "leading past the half pixel rounds up",
                (8.0, 12.0, 4.0, 2.6),
                (8, 19, 12),
            ),
            // DejaVu Sans Mono's `M` at 14 pt / 96 dpi.
            (
                "11.24 px advance yields an 11 px cell",
                (11.24, 12.0, 4.0, 0.0),
                (11, 16, 12),
            ),
            // Menlo's `M` at 14 pt / 72 dpi.
            (
                "8.43 px advance yields an 8 px cell",
                (8.43, 12.0, 4.0, 0.0),
                (8, 16, 12),
            ),
            (
                "advance past the half pixel rounds up",
                (16.86, 12.0, 4.0, 0.0),
                (17, 16, 12),
            ),
            (
                "whole pixel advance keeps its width",
                (7.0, 12.0, 4.0, 0.0),
                (7, 16, 12),
            ),
            (
                "sub-half-pixel advance still yields one pixel",
                (0.3, 12.0, 4.0, 0.0),
                (1, 16, 12),
            ),
            (
                "degenerate metrics still yield a one pixel cell",
                (0.0, 0.0, 0.0, 0.0),
                (1, 1, 0),
            ),
        ];
        for (case, (width, ascent, descent, leading), expected) in cases {
            let metrics = CellMetrics::from_scaled(width, ascent, descent, leading);
            assert_eq!(
                (metrics.width, metrics.height, metrics.ascent),
                expected,
                "{case}"
            );
        }
    }

    #[test]
    fn rasterizes_capital_a() {
        let font = font();
        let mut cache = ShapeCache::default();
        let bitmap = cache.get_or_insert(&font, 'A', 16);
        assert!(
            bitmap.width() > 0 && bitmap.height() > 0,
            "A should rasterize"
        );
        assert!(!bitmap.is_blank(), "A should have non-zero pixels");
    }

    /// A mask glyph comes back as `Coverage`; the renderer routes it to
    /// the `R8` atlas on this tag.
    #[test]
    fn mask_glyph_rasterizes_as_coverage() {
        let font = font();
        let bitmap = rasterize(&mut ScaleContext::new(), &font, 'A', 16.0);
        assert!(
            matches!(bitmap.pixels(), GlyphPixels::Coverage(_)),
            "ASCII 'A' is a coverage mask, not color",
        );
    }

    /// A symbol only the color-emoji font covers (⏸ U+23F8) rasterizes
    /// as `Rgba`, not collapsed to a mask (the ⏸/⏺ solid-square
    /// regression). Skips when no color face covers it.
    #[test]
    fn color_emoji_rasterizes_as_rgba_not_a_mask() {
        let stack = stack();
        let font = stack.resolve('⏸', FontStyle::REGULAR);
        if !font.has_glyph('⏸') {
            eprintln!("no font covers U+23F8 on this host; skipping color-emoji test");
            return;
        }
        if !font.has_color_glyphs() {
            eprintln!("U+23F8 resolves to a monochrome face on this host; skipping");
            return;
        }
        let bitmap = rasterize(&mut ScaleContext::new(), font, '⏸', 32.0);
        assert!(
            bitmap.is_color(),
            "a color-face U+23F8 must rasterize as RGBA, not collapse to a mask",
        );
    }

    /// A base plus combining mark shapes without panicking. The GPOS
    /// offset is font-dependent, so no portable offset assertion exists.
    #[test]
    fn combining_mark_cluster_shapes_without_panicking() {
        let stack = stack();
        let mut shaper = Shaper::new();
        let shaping = shaper.shape_cluster(&stack, 32.0, FontStyle::REGULAR, "a\u{0301}");
        assert!(
            !shaping.glyphs.is_empty(),
            "a mark cluster must shape to at least the base glyph",
        );
    }

    /// ❤️‍🔥 shapes against the color-emoji face, not the mono face a bare
    /// U+2764 resolves to, or GSUB never forms the ligature. Skips when
    /// the host has no distinct emoji face.
    #[test]
    fn emoji_zwj_with_text_default_base_shapes_against_emoji_face() {
        let stack = stack();
        let heart_face = stack.resolve_index('\u{2764}');
        let emoji_face = stack.resolve_index('\u{1F525}');
        if heart_face == emoji_face {
            eprintln!("host lacks a distinct emoji face for U+1F525; skipping face-anchor test");
            return;
        }
        let mut shaper = Shaper::new();
        let cluster = shaper.shape_cluster(
            &stack,
            32.0,
            FontStyle::REGULAR,
            "\u{2764}\u{FE0F}\u{200D}\u{1F525}",
        );
        assert_eq!(
            cluster.font_id, emoji_face,
            "heart-on-fire must resolve to the emoji face, not the mono heart face"
        );
    }

    /// A bare ❤️ (VS16) resolves to the color face; the same base without
    /// VS16 keeps its mono face.
    #[test]
    fn standalone_vs16_symbol_prefers_the_color_face() {
        let stack = stack();
        let plain = stack.resolve_styled_index('\u{2764}', FontStyle::REGULAR);
        let Some(color_face) = stack.resolve_color_index('\u{2764}', FontStyle::REGULAR) else {
            eprintln!("host has no color face covering U+2764; skipping VS16 color test");
            return;
        };
        if plain == color_face {
            eprintln!("U+2764 already resolves color on this host; nothing to correct");
            return;
        }
        let mut shaper = Shaper::new();
        let with_vs16 = shaper.shape_cluster(&stack, 32.0, FontStyle::REGULAR, "\u{2764}\u{FE0F}");
        assert_eq!(
            with_vs16.font_id, color_face,
            "❤️ (VS16) must resolve to the color face"
        );
        let no_vs16 = shaper.shape_cluster(&stack, 32.0, FontStyle::REGULAR, "\u{2764}");
        assert_eq!(
            no_vs16.font_id, plain,
            "a bare ❤ without VS16 must stay on its text-presentation face"
        );
    }

    #[test]
    fn cache_returns_stable_results() {
        let font = font();
        let mut cache = ShapeCache::default();
        let first = cache.get_or_insert(&font, 'X', 16).clone();
        let second = cache.get_or_insert(&font, 'X', 16).clone();
        assert_eq!(first.pixels.as_bytes(), second.pixels.as_bytes());
        assert_eq!(cache.len(), 1);
    }

    /// Once `bytes_used` crosses the cap, inserts evict other entries
    /// but keep the just-inserted one.
    #[test]
    fn byte_cap_evicts_old_entries_but_keeps_the_newest() {
        let font = font();
        let mut cache = ShapeCache::new(1);

        cache.get_or_insert(&font, 'A', 16);
        assert_eq!(cache.len(), 1, "first insert is never evicted");

        for c in ['B', 'C', 'D', 'E'] {
            cache.get_or_insert(&font, c, 16);
            assert_eq!(
                cache.len(),
                1,
                "tiny cap holds at most the just-inserted entry",
            );
        }

        let last = ('E', 16u32, SizingKey::default());
        assert!(
            cache.map.contains_key(&last),
            "newest key survives eviction",
        );
        assert!(
            cache.bytes_used <= entry_cost(&cache.map[&last]),
            "accounting reflects only the surviving entry",
        );
    }

    /// A cap wide enough for several entries is trimmed to the cap, not
    /// flushed. Five sizings of one glyph cost the same bytes, so the
    /// surviving count is exact.
    #[test]
    fn byte_cap_evicts_only_down_to_the_cap_when_it_holds_several_entries() {
        let font = font();
        let m = ShapeCache::default().get_or_insert(&font, 'M', 32).clone();
        assert!(!m.is_blank(), "'M' must rasterize to a non-empty bitmap");
        let entry_bytes = entry_cost(&m);
        let mut cache = ShapeCache::new(entry_bytes * 3);

        for scale in 1..=5u8 {
            cache.get_or_insert_sized(&font, 'M', 32, SizingKey::new(scale, 0, 0, 0, 0, 0));
        }
        assert_eq!(
            cache.len(),
            3,
            "a three-entry cap evicts down to three, not to the newest alone",
        );
        assert_eq!(
            cache.bytes_used,
            entry_bytes * 3,
            "accounting must track exactly the surviving entries",
        );
        assert!(
            cache
                .map
                .contains_key(&('M', 32, SizingKey::new(5, 0, 0, 0, 0, 0))),
            "the just-inserted entry is excluded from eviction",
        );
    }

    #[test]
    fn blank_bitmaps_count_against_the_byte_cap() {
        let font = font();
        let cap = 4096;
        let mut cache = ShapeCache::new(cap);
        for w in 0..=7u8 {
            for n in 0..=15u8 {
                for d in 0..=15u8 {
                    let bitmap =
                        cache.get_or_insert_sized(&font, ' ', 16, SizingKey::new(1, w, n, d, 0, 0));
                    assert!(bitmap.is_blank());
                }
            }
        }
        assert!(
            cache.len() <= cap / ENTRY_BYTES,
            "{} blank entries under a {cap}-byte cap",
            cache.len(),
        );
    }

    /// Distinct `SizingKey` values produce distinct cache entries.
    #[test]
    fn sizing_key_prevents_collisions() {
        let font = font();
        let mut cache = ShapeCache::default();
        cache.get_or_insert(&font, 'A', 16);
        assert_eq!(cache.len(), 1);

        let scale_2 = SizingKey::new(2, 0, 0, 0, 0, 0);
        cache.get_or_insert_sized(&font, 'A', 16, scale_2);
        assert_eq!(cache.len(), 2);

        let scale_2_bottom = SizingKey::new(2, 0, 0, 0, 1, 0);
        cache.get_or_insert_sized(&font, 'A', 16, scale_2_bottom);
        assert_eq!(cache.len(), 3);

        cache.get_or_insert_sized(&font, 'A', 16, scale_2);
        assert_eq!(cache.len(), 3);
    }

    /// `effective_scale` is `s × n/d`, or `s` alone without a
    /// fractional part.
    #[test]
    fn sizing_key_effective_scale_honors_fractional_n_over_d() {
        assert!((SizingKey::new(1, 0, 1, 2, 0, 0).effective_scale() - 0.5).abs() < 1e-5);
        assert!((SizingKey::new(2, 0, 1, 2, 0, 0).effective_scale() - 1.0).abs() < 1e-5);
        assert!((SizingKey::new(1, 0, 0, 0, 0, 0).effective_scale() - 1.0).abs() < 1e-5);
        assert!((SizingKey::new(7, 0, 14, 15, 0, 0).effective_scale() - 6.533_333_3).abs() < 1e-5);
        assert!((SizingKey::default().effective_scale() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn default_sizing_wrapper_shares_an_entry_with_explicit_default() {
        let font = font();
        let mut cache = ShapeCache::default();
        cache.get_or_insert(&font, 'B', 16);
        cache.get_or_insert_sized(&font, 'B', 16, SizingKey::default());
        cache.get_or_insert_sized(&font, 'B', 16, SizingKey::new(1, 0, 0, 0, 0, 0));
        assert_eq!(cache.len(), 1);
        assert_eq!(SizingKey::default(), SizingKey::new(1, 0, 0, 0, 0, 0));
    }

    #[test]
    fn space_is_blank() {
        let font = font();
        let mut cache = ShapeCache::default();
        let bitmap = cache.get_or_insert(&font, ' ', 16);
        assert!(bitmap.is_blank());
    }

    /// A typo in `font.family` falls back to `Family::Monospace` and
    /// picks the same face an unset `font.family` would.
    #[test]
    fn unknown_primary_family_falls_back_to_monospace() {
        let fallback = FontStack::auto_discover(
            Some("definitely-not-a-real-family-xyz123"),
            &[],
            &[],
            &StyleFaces::default(),
        )
        .expect("fallback to monospace");
        let baseline = stack();
        // Metrics stand in for face identity; the `Arc`s are distinct loads.
        assert_eq!(
            fallback.primary().cell_metrics(16.0).width,
            baseline.primary().cell_metrics(16.0).width
        );
    }

    #[test]
    fn default_monospace_is_no_font_only_when_no_face_is_installed() {
        let result =
            FontStack::auto_discover_in(&Database::new(), None, &[], &[], &StyleFaces::default());
        assert!(matches!(result, Err(ShapingError::NoFont)));
    }

    /// Debian's fontconfig rules leave fontdb's `monospace` generic naming
    /// `FreeMono`, which the host need not install.
    #[test]
    fn uninstalled_monospace_generic_falls_back_to_an_installed_fixed_pitch_family() {
        let Some(dir) = std::env::var_os("FELIS_TEST_FONT_DIR") else {
            eprintln!("FELIS_TEST_FONT_DIR unset; skipping");
            return;
        };
        let mut db = Database::new();
        db.load_fonts_dir(std::path::PathBuf::from(dir));
        db.set_monospace_family("FreeMono");

        let (id, family) = query_default_monospace(&db).expect("a fixed-pitch face is installed");
        let face = db.face(id).expect("resolved id names a face");
        assert!(face.monospaced);
        assert_eq!(face.families[0].0, family);
        assert_eq!(face.weight, fontdb::Weight::NORMAL);
        assert_eq!(face.style, fontdb::Style::Normal);

        let stack = FontStack::auto_discover_in(&db, None, &[], &[], &StyleFaces::default())
            .expect("default config loads");
        assert!(
            !Arc::ptr_eq(
                stack.styled_primary(FontStyle::REGULAR),
                stack.styled_primary(FontStyle {
                    bold: true,
                    italic: false,
                })
            ),
            "bold derives from the fallback family, not the unresolved generic",
        );
    }

    #[test]
    fn has_glyph_true_for_ascii_in_monospace_font() {
        let font = font();
        for c in ['A', 'a', '0', '!', ' ', 'M'] {
            assert!(font.has_glyph(c), "monospace must cover ASCII {c:?}");
        }
    }

    #[test]
    fn has_glyph_false_for_unmapped_private_use_codepoint() {
        let font = font();
        assert!(!font.has_glyph('\u{F8FFD}'));
    }

    #[test]
    fn stack_with_only_primary_resolves_to_primary() {
        let primary = Arc::new(font());
        let stack = FontStack::new(primary.clone());
        assert!(Arc::ptr_eq(
            stack.resolve('A', FontStyle::REGULAR),
            &primary
        ));
        assert!(Arc::ptr_eq(
            stack.resolve('\u{F8FFD}', FontStyle::REGULAR),
            &primary
        ));
        assert_eq!(stack.len(), 1);
    }

    #[test]
    fn stack_resolve_picks_first_match() {
        let primary = Arc::new(font());
        // The same face twice, so the test needs no second font in CI.
        let secondary = Arc::new(font());
        let stack = FontStack::new(primary.clone()).with_fallback_features(secondary, Vec::new());
        assert!(
            Arc::ptr_eq(stack.resolve('A', FontStyle::REGULAR), &primary),
            "primary must win when both have the glyph"
        );
        assert_eq!(stack.len(), 2);
    }

    #[test]
    fn stack_resolve_walks_past_primary_when_it_lacks_glyph() {
        let primary = Arc::new(font());
        let cjk_target = '\u{3042}';
        if primary.has_glyph(cjk_target) {
            eprintln!("primary monospace covers Hiragana — fallback walk untestable here");
            return;
        }
        let db = system_db();
        let mut cjk: Option<Arc<Font>> = None;
        for &name in CJK_FALLBACK_FAMILIES {
            if let Ok(font) = Font::try_load_with(&db, name) {
                cjk = Some(Arc::new(font));
                break;
            }
        }
        let Some(cjk) = cjk else {
            eprintln!("no CJK fallback installed; skipping fallback-walk assertion");
            return;
        };
        let stack = FontStack::new(primary).with_fallback_features(cjk.clone(), Vec::new());
        assert!(
            Arc::ptr_eq(stack.resolve(cjk_target, FontStyle::REGULAR), &cjk),
            "fallback must win when primary lacks the glyph"
        );
    }

    /// Text-presentation-default symbols (✳ ✻ …) resolve mono, not to
    /// the color-emoji face. Runs the real discovery order against the
    /// pinned devshell set (Monaspace + Noto Sans Symbols 2 + Noto Color
    /// Emoji, which also covers them); the host's own fontconfig may lack
    /// a mono home and legitimately go color.
    #[test]
    fn text_presentation_dingbats_resolve_to_mono_not_color() {
        let Some(dir) = std::env::var_os("FELIS_TEST_FONT_DIR") else {
            eprintln!("FELIS_TEST_FONT_DIR unset; skipping");
            return;
        };
        let mut db = Database::new();
        db.load_fonts_dir(std::path::PathBuf::from(dir));
        // The pinned db has no fontconfig generic-family aliases, so
        // `Family::Monospace` would not resolve.
        let stack = FontStack::auto_discover_in(
            &db,
            Some(TEST_FONT_FAMILY),
            &[],
            &[],
            &StyleFaces::default(),
        )
        .expect("Monaspace primary must load from the pinned font dir");
        let mut ctx = ScaleContext::new();
        for c in ['✳', '✻', '✶', '✢'] {
            let font = stack.resolve(c, FontStyle::REGULAR);
            assert!(
                font.has_glyph(c),
                "the pinned symbol face must cover U+{:04X}",
                c as u32,
            );
            let bitmap = rasterize(&mut ctx, font, c, 32.0);
            assert!(
                !bitmap.is_color(),
                "U+{:04X} must rasterize as a monochrome glyph, got color \
                 (symbol fallback should win before the emoji font)",
                c as u32,
            );
        }
    }

    #[test]
    fn auto_discover_returns_at_least_the_primary() {
        let stack = FontStack::auto_discover(None, &[], &[], &StyleFaces::default())
            .expect("primary must load");
        assert!(!stack.is_empty());
        assert!(stack.primary().has_glyph('A'));
    }

    #[test]
    fn explicit_fallbacks_replace_auto_discover() {
        let stack = FontStack::auto_discover(
            None,
            &[],
            &[FaceSpec::named("definitely-not-a-real-family-xyz123")],
            &StyleFaces::default(),
        )
        .expect("primary must load");
        assert_eq!(stack.len(), 1);
    }

    fn test_font_files() -> Option<Vec<std::path::PathBuf>> {
        let dir = std::path::PathBuf::from(std::env::var_os("FELIS_TEST_FONT_DIR")?);
        let files = std::fs::read_dir(dir.join("truetype"))
            .expect("FELIS_TEST_FONT_DIR has a truetype directory")
            .map(|entry| entry.expect("readable directory entry").path())
            .filter(|path| path.to_string_lossy().contains("MonaspaceNeon"))
            .collect();
        Some(files)
    }

    /// A fallback the system may well have (`DejaVu Sans Mono`) is absent from the
    /// pinned files, so it must not join the stack.
    #[test]
    fn discover_in_files_resolves_from_the_given_fonts_alone() {
        let Some(files) = test_font_files() else {
            eprintln!("FELIS_TEST_FONT_DIR unset; skipping");
            return;
        };
        let stack = FontStack::discover_in_files(
            &files,
            Some(TEST_FONT_FAMILY),
            &[],
            &[FaceSpec::named("DejaVu Sans Mono")],
            &StyleFaces::default(),
        )
        .expect("the pinned family loads");
        let pinned = Font::try_load_test_font().expect("FELIS_TEST_FONT_DIR is set");
        let dims = |m: CellMetrics| (m.width, m.height, m.ascent);
        assert_eq!(
            dims(stack.primary().cell_metrics(14.0)),
            dims(pinned.cell_metrics(14.0))
        );
        assert_eq!(stack.len(), 1);
    }

    #[test]
    fn discover_in_files_reports_a_file_that_does_not_load() {
        let missing = std::path::PathBuf::from("/nonexistent/felis-font.ttf");
        let err = FontStack::discover_in_files(
            std::slice::from_ref(&missing),
            None,
            &[],
            &[],
            &StyleFaces::default(),
        )
        .err()
        .expect("a missing file cannot load");
        assert!(matches!(err, ShapingError::FontPath { path, .. } if path == missing));
    }

    /// fontdb accepts a readable file with no face in it without error.
    #[test]
    fn discover_in_files_reports_a_file_with_no_face() {
        let Some(mut files) = test_font_files() else {
            eprintln!("FELIS_TEST_FONT_DIR unset; skipping");
            return;
        };
        let not_a_font =
            std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"));
        files.push(not_a_font.clone());
        let err = FontStack::discover_in_files(
            &files,
            Some(TEST_FONT_FAMILY),
            &[],
            &[],
            &StyleFaces::default(),
        )
        .err()
        .expect("a file with no face is rejected even when the others load");
        assert!(matches!(err, ShapingError::FontPath { path, .. } if path == not_a_font));
    }

    #[test]
    fn auto_discover_inherits_primary_features_into_fallbacks() {
        let features = vec!["calt".to_owned(), "-clig".to_owned()];
        let stack = FontStack::auto_discover(None, &features, &[], &StyleFaces::default())
            .expect("primary must load");
        assert_eq!(stack.features_at(0), features.as_slice());
        for i in 0..stack.len() {
            assert_eq!(
                stack.features_at(i),
                features.as_slice(),
                "fallback index {i} must inherit the primary's features",
            );
        }
    }

    #[test]
    fn explicit_fallback_with_empty_features_opts_out() {
        let primary_features = vec!["calt".to_owned(), "ss01".to_owned()];
        let db = system_db();
        let mut probe_family: Option<String> = None;
        for &name in CJK_FALLBACK_FAMILIES
            .iter()
            .chain(EMOJI_FALLBACK_FAMILIES.iter())
            .chain(NERD_FONT_FAMILIES.iter())
        {
            if Font::try_load_with(&db, name).is_ok() {
                probe_family = Some(name.to_owned());
                break;
            }
        }
        let Some(family) = probe_family else {
            eprintln!("no installable fallback family found; skipping opt-out assertion");
            return;
        };
        let stack = FontStack::auto_discover(
            None,
            &primary_features,
            &[FaceSpec {
                family: Some(family),
                features: Some(Vec::new()),
            }],
            &StyleFaces::default(),
        )
        .expect("primary must load");
        assert_eq!(stack.features_at(0), primary_features.as_slice());
        assert_eq!(stack.len(), 2, "primary + one fallback");
        let empty: &[String] = &[];
        assert_eq!(
            stack.features_at(1),
            empty,
            "explicit empty features is an opt-out, not an inherit"
        );
    }

    #[test]
    fn explicit_fallback_without_features_inherits_primary() {
        let db = system_db();
        let Some(family) = CJK_FALLBACK_FAMILIES
            .iter()
            .chain(EMOJI_FALLBACK_FAMILIES.iter())
            .chain(NERD_FONT_FAMILIES.iter())
            .find(|name| Font::try_load_with(&db, name).is_ok())
        else {
            eprintln!("no installable fallback family found; skipping inherit assertion");
            return;
        };
        let primary_features = vec!["calt".to_owned(), "ss01".to_owned()];
        let stack = FontStack::auto_discover(
            None,
            &primary_features,
            &[FaceSpec::named(*family)],
            &StyleFaces::default(),
        )
        .expect("primary must load");
        assert_eq!(stack.len(), 2, "primary + one fallback");
        assert_eq!(
            stack.features_at(1),
            primary_features.as_slice(),
            "an omitted features list inherits, it does not opt out"
        );
    }

    #[test]
    fn explicit_fallback_with_features_overrides_primary() {
        let db = system_db();
        let mut probe_family: Option<String> = None;
        for &name in CJK_FALLBACK_FAMILIES {
            if Font::try_load_with(&db, name).is_ok() {
                probe_family = Some(name.to_owned());
                break;
            }
        }
        let Some(family) = probe_family else {
            eprintln!("no CJK fallback installed; skipping override assertion");
            return;
        };
        let override_features = vec!["palt".to_owned(), "trad".to_owned()];
        let stack = FontStack::auto_discover(
            None,
            &["calt".to_owned()],
            &[FaceSpec {
                family: Some(family),
                features: Some(override_features.clone()),
            }],
            &StyleFaces::default(),
        )
        .expect("primary must load");
        assert_eq!(stack.features_at(0), &["calt".to_owned()]);
        assert_eq!(stack.features_at(1), override_features.as_slice());
    }

    /// Feature lists follow the face `font_at` picks; a style with no
    /// override inherits the base list.
    #[test]
    fn styled_feature_override_applies_where_the_styled_face_does() {
        let base = vec!["calt".to_owned()];
        let bold_features = vec!["-liga".to_owned()];
        let stack = FontStack::auto_discover(
            None,
            &base,
            &[],
            &StyleFaces {
                bold: &FaceSpec {
                    family: None,
                    features: Some(bold_features.clone()),
                },
                italic: &FaceSpec {
                    family: None,
                    features: None,
                },
                bold_italic: &FaceSpec {
                    family: None,
                    features: None,
                },
            },
        )
        .expect("primary must load");

        let regular = FontStyle {
            bold: false,
            italic: false,
        };
        let bold = FontStyle {
            bold: true,
            italic: false,
        };
        let italic = FontStyle {
            bold: false,
            italic: true,
        };
        assert_eq!(stack.features_for(0, regular), base.as_slice());
        assert_eq!(stack.features_for(0, bold), bold_features.as_slice());
        assert_eq!(
            stack.features_for(0, italic),
            base.as_slice(),
            "a style with no override inherits the base features",
        );
    }

    #[test]
    fn shape_run_with_empty_text_produces_no_glyphs() {
        let font = font();
        let mut shaper = Shaper::new();
        let out = shaper.shape_run(&font, 16.0, &[], "");
        assert_eq!(out, []);
    }

    /// Features-off shaping is one glyph per byte, or the renderer could
    /// not opt cells into shaping without shifting the grid.
    #[test]
    fn shape_run_ascii_no_features_emits_one_glyph_per_byte() {
        let font = font();
        let mut shaper = Shaper::new();
        let out = shaper.shape_run(&font, 16.0, &[], "Hello");
        assert_eq!(out.len(), 5, "one glyph per ASCII byte");
        for (i, g) in out.iter().enumerate() {
            assert_eq!(g.source_byte_start, i as u32);
            assert_eq!(g.source_byte_len, 1);
            assert!(g.advance_px > 0.0, "ASCII glyph must have non-zero advance");
            assert!(!g.from_ligature, "no features → no ligature substitutions");
            assert_ne!(g.glyph_id, 0, "ASCII char must map to a real glyph");
        }
    }

    #[test]
    fn shape_run_of_spaces_produces_one_glyph_per_cell() {
        let font = font();
        let mut shaper = Shaper::new();
        let out = shaper.shape_run(&font, 16.0, &[], "    ");
        assert_eq!(out.len(), 4);
        for g in &out {
            assert!(g.advance_px > 0.0, "space must advance the pen");
        }
    }

    /// Malformed feature strings are dropped silently and the output
    /// equals the empty-feature-list output.
    #[test]
    fn shape_run_rejects_malformed_features_without_failing() {
        let font = font();
        let mut shaper = Shaper::new();
        let baseline = shaper.shape_run(&font, 16.0, &[], "ABC");
        let with_garbage = shaper.shape_run(
            &font,
            16.0,
            &["nope".to_owned(), "xx".to_owned(), String::new()],
            "ABC",
        );
        assert_eq!(baseline.len(), with_garbage.len());
        for (a, b) in baseline.iter().zip(with_garbage.iter()) {
            assert_eq!(a.glyph_id, b.glyph_id);
        }
    }

    /// The char path and the glyph-id path produce identical bitmaps, so
    /// mixed-mode rendering never draws one letter two ways.
    #[test]
    fn rasterize_by_char_matches_rasterize_by_glyph_id() {
        let font = font();
        let face = font.font_ref();
        let glyph_id = face.charmap().map('A');
        assert_ne!(glyph_id, 0, "system monospace must cover 'A'");
        let mut cache = ShapeCache::default();
        let from_char = rasterize(&mut cache.scale_ctx, &font, 'A', 16.0);
        let from_id = cache.rasterize_glyph_id(&font, glyph_id, 16.0);
        assert_eq!(from_char.pixels.as_bytes(), from_id.pixels.as_bytes());
        assert_eq!(from_char.width(), from_id.width());
        assert_eq!(from_char.height(), from_id.height());
        assert_eq!(from_char.left, from_id.left);
        assert_eq!(from_char.top, from_id.top);
    }

    /// `rasterize_glyph_id(0)` returns a blank bitmap, matching the char
    /// path's charmap miss.
    #[test]
    fn rasterize_glyph_id_zero_returns_blank() {
        let font = font();
        let bitmap = ShapeCache::default().rasterize_glyph_id(&font, 0, 16.0);
        assert!(bitmap.is_blank());
        assert_eq!(bitmap.width(), 0);
        assert_eq!(bitmap.height(), 0);
    }

    /// `font_ref()` hands swash the same `CacheKey` every call (swash
    /// keys its per-font caches on it) and distinct fonts never share one.
    #[test]
    fn font_ref_key_is_stable_per_font_and_distinct_between_fonts() {
        let font = font();
        assert_eq!(
            font.font_ref().key,
            font.font_ref().key,
            "font_ref() must hand swash the same cache key every call",
        );
        let other = self::font();
        assert_ne!(
            font.font_ref().key,
            other.font_ref().key,
            "separately-loaded fonts must not share a swash cache slot",
        );
    }

    #[test]
    fn shape_run_is_repeatable_across_calls_on_the_same_shaper() {
        let font = font();
        let mut shaper = Shaper::new();
        let features = vec!["calt".to_owned()];
        let first = shaper.shape_run(&font, 16.0, &features, "Hello");
        let second = shaper.shape_run(&font, 16.0, &features, "Hello");
        assert_eq!(first.len(), second.len());
        for (a, b) in first.iter().zip(second.iter()) {
            assert_eq!(a.glyph_id, b.glyph_id);
            assert_eq!(a.source_byte_start, b.source_byte_start);
            assert_eq!(a.source_byte_len, b.source_byte_len);
            assert!((a.advance_px - b.advance_px).abs() < f32::EPSILON);
            assert_eq!(a.from_ligature, b.from_ligature);
        }
    }

    /// Monaspace's `calt` texture healing is on by default (swash, like
    /// `HarfBuzz` and CSS, applies `calt` unrequested) and `-calt` turns
    /// it off. Needs the pinned devshell font: `FiraCode`'s calt never
    /// yields a single ligature glyph, and an uninstalled probe font
    /// skips silently.
    #[test]
    fn shape_run_calt_texture_healing_defaults_on_and_disables() {
        let Some(font) = Font::try_load_test_font() else {
            eprintln!("FELIS_TEST_FONT_DIR unset; skipping");
            return;
        };
        let charmap = font.font_ref().charmap();
        let base_ids = vec![charmap.map('m'), charmap.map('i'), charmap.map('m')];
        let mut shaper = Shaper::new();
        let healed = shaper.shape_run(&font, 16.0, &[], "mim");
        let disabled = shaper.shape_run(&font, 16.0, &["-calt".to_owned()], "mim");
        for out in [&healed, &disabled] {
            assert_eq!(out.len(), 3, "healing must keep one glyph per cell");
            assert!(out.iter().all(|g| g.source_byte_len == 1));
        }
        let healed_ids: Vec<u16> = healed.iter().map(|g| g.glyph_id).collect();
        let disabled_ids: Vec<u16> = disabled.iter().map(|g| g.glyph_id).collect();
        assert_eq!(
            disabled_ids, base_ids,
            "`-calt` must fall back to the unhealed charmap glyphs",
        );
        assert_ne!(
            healed_ids, base_ids,
            "default shaping must apply calt texture healing",
        );
    }

    /// The documented `font.features` grammar, over every 4-byte tag:
    /// swash's `Setting::parse` returns `None` for every bare tag, so
    /// each surface form here is one felis must accept itself.
    fn arb_tag() -> impl Strategy<Value = String> {
        proptest::string::string_regex("[a-z0-9]{4}").expect("literal tag regex")
    }

    proptest! {
        #[test]
        fn parse_feature_accepts_every_documented_surface_form(
            tag in arb_tag(),
            value in any::<u16>(),
            pad_left in "[ ]{0,2}",
            pad_right in "[ ]{0,2}",
            quote in prop_oneof![Just(""), Just("'"), Just("\"")],
        ) {
            let raw = u32::from_be_bytes(tag.as_bytes().try_into().expect("4-byte tag"));
            let quoted = format!("{quote}{tag}{quote}");
            let wrap = |entry: &str| format!("{pad_left}{entry}{pad_right}");
            for (entry, want) in [
                (wrap(&quoted), 1),
                (wrap(&format!("-{quoted}")), 0),
                (wrap(&format!("{quoted} on")), 1),
                (wrap(&format!("{quoted} off")), 0),
                (wrap(&format!("{quoted} {value}")), value),
            ] {
                let s = parse_feature(&entry).ok_or_else(|| {
                    TestCaseError::fail(format!("{entry:?} must parse"))
                })?;
                prop_assert_eq!((s.tag, s.value), (raw, want), "entry {:?}", entry);
            }
        }

        #[test]
        fn parse_feature_rejects_negation_with_a_value_and_oversize_tags(
            tag in arb_tag(),
            value in any::<u16>(),
            extra in "[a-z0-9]{1,4}",
        ) {
            prop_assert_eq!(parse_feature(&format!("-{tag} {value}")), None);
            prop_assert_eq!(parse_feature(&format!("-{tag} off")), None);
            prop_assert_eq!(parse_feature(&format!("{tag}{extra}")), None);
            prop_assert_eq!(parse_feature(&format!("{tag} {value} {value}")), None);
        }
    }

    #[test]
    fn parse_feature_rejects_malformed_entries() {
        for bad in [
            "",
            "   ",
            "-",
            "ligatures",  // > 4 bytes
            "ss03 maybe", // bad value token
            "ss03 1 2",   // trailing garbage
            "-clig off",  // negation + explicit value contradict
            "ss03 65536", // u16 overflow
            "ss\u{30c3}", // non-ASCII tag
        ] {
            assert!(parse_feature(bad).is_none(), "{bad:?} must not parse");
        }
    }

    /// A bare `"ss03"` must reach GSUB: Monaspace keeps its arrows in
    /// `ss03`, off by default, so a glyph-id change proves the feature
    /// arrived (a `calt` check passes vacuously). Also pins the two-glyph
    /// blank-lead + body form `apply_shaped_run` in felis-render-wgpu
    /// depends on.
    #[test]
    fn shape_run_applies_non_default_stylistic_set() {
        let Some(font) = Font::try_load_test_font() else {
            eprintln!("FELIS_TEST_FONT_DIR unset; skipping");
            return;
        };
        let mut shaper = Shaper::new();
        let plain = shaper.shape_run(&font, 16.0, &[], "->");
        let ss03 = shaper.shape_run(&font, 16.0, &["ss03".to_owned()], "->");
        let plain_ids: Vec<u16> = plain.iter().map(|g| g.glyph_id).collect();
        let ss03_ids: Vec<u16> = ss03.iter().map(|g| g.glyph_id).collect();
        assert_ne!(
            plain_ids, ss03_ids,
            "ss03 must swap the `->` glyphs for the arrow alternates",
        );
        assert_eq!(ss03.len(), 2, "Monaspace arrows are blank-lead + body");
        assert!(
            ss03.iter()
                .all(|g| g.from_ligature && g.source_byte_len == 2),
            "both arrow pieces must claim the full 2-byte cluster",
        );
    }

    #[test]
    fn font_style_index_roundtrips() {
        for i in 0..4 {
            assert_eq!(FontStyle::from_index(i).index(), i);
        }
        assert_eq!(FontStyle::REGULAR, FontStyle::default());
        assert_eq!(FontStyle::REGULAR.index(), 0);
    }

    /// Both boundaries are load-bearing: U+20000 (CJK Extension B) must
    /// not anchor, and BMP text-default symbols must not be forced to
    /// color.
    #[test]
    fn emoji_face_anchor_spans_the_astral_pictographic_range_only() {
        assert!(is_emoji_face_anchor('\u{1F000}'), "first mahjong tile");
        assert!(is_emoji_face_anchor('\u{1FAFF}'), "last extended-A scalar");
        assert!(is_emoji_face_anchor('\u{1F525}'), "🔥, the ZWJ anchor case");
        assert!(!is_emoji_face_anchor('\u{1EFFF}'), "one below the range");
        assert!(
            !is_emoji_face_anchor('\u{1FB00}'),
            "symbols for legacy computing are mono, not emoji",
        );
        assert!(!is_emoji_face_anchor('\u{20000}'), "CJK Extension B");
        assert!(
            !is_emoji_face_anchor('\u{2764}'),
            "a text-default heart must not force the emoji face",
        );
        assert!(!is_emoji_face_anchor('A'));
    }

    /// `primary_covers` asks the styled primary, not `fonts[0]`; the
    /// renderer breaks a shape run when the styled face stops covering.
    /// Bold is pinned to Noto Sans Symbols 2 (⌨, no Latin) against
    /// Monaspace Neon (the reverse).
    #[test]
    fn primary_covers_answers_from_the_styled_primary_not_the_regular_one() {
        let Some(dir) = std::env::var_os("FELIS_TEST_FONT_DIR") else {
            eprintln!("FELIS_TEST_FONT_DIR unset; skipping");
            return;
        };
        let mut db = Database::new();
        db.load_fonts_dir(std::path::PathBuf::from(dir));
        let base = Family::Name(TEST_FONT_FAMILY);
        let regular_id = db
            .query(&Query {
                families: &[base],
                ..Query::default()
            })
            .expect("regular Monaspace Neon must resolve");
        let regular = Arc::new(Font::load_face_id(&db, regular_id).expect("load regular face"));
        let bold = FontStyle {
            bold: true,
            italic: false,
        };
        let symbols = FaceSpec::named("Noto Sans Symbols 2");
        let mut stack = FontStack::with_primary_features(regular.clone(), Vec::new());
        stack.load_styled(
            &db,
            regular_id,
            &regular,
            base,
            &[],
            &StyleFaces {
                bold: &symbols,
                ..StyleFaces::default()
            },
        );
        assert!(
            !Arc::ptr_eq(stack.styled_primary(bold), stack.primary()),
            "the bold override must have loaded a face of its own",
        );

        assert!(stack.primary_covers('A', FontStyle::REGULAR));
        assert!(
            !stack.primary_covers('A', bold),
            "a bold 'A' the bold face lacks must report uncovered",
        );
        assert!(stack.primary_covers('\u{2328}', bold));
        assert!(!stack.primary_covers('\u{2328}', FontStyle::REGULAR));
    }

    /// Styled primaries load distinct faces when the family ships them,
    /// and a missing override dedups to the regular `Arc`.
    #[test]
    fn styled_primaries_pick_real_faces_and_dedup_missing_to_regular() {
        let Some(dir) = std::env::var_os("FELIS_TEST_FONT_DIR") else {
            eprintln!("FELIS_TEST_FONT_DIR unset; skipping");
            return;
        };
        let mut db = Database::new();
        db.load_fonts_dir(std::path::PathBuf::from(dir));
        let base = Family::Name(TEST_FONT_FAMILY);
        let regular_id = db
            .query(&Query {
                families: &[base],
                ..Query::default()
            })
            .expect("regular Monaspace Neon must resolve");
        let regular = Arc::new(Font::load_face_id(&db, regular_id).expect("load regular face"));

        let bold = FontStyle {
            bold: true,
            italic: false,
        };
        let italic = FontStyle {
            bold: false,
            italic: true,
        };
        let bold_italic = FontStyle {
            bold: true,
            italic: true,
        };

        let mut stack = FontStack::with_primary_features(regular.clone(), Vec::new());
        stack.load_styled(&db, regular_id, &regular, base, &[], &StyleFaces::default());
        assert!(
            !Arc::ptr_eq(
                stack.styled_primary(FontStyle::REGULAR),
                stack.styled_primary(bold)
            ),
            "Monaspace Neon ships a real Bold face distinct from Regular",
        );
        assert!(
            !Arc::ptr_eq(
                stack.styled_primary(FontStyle::REGULAR),
                stack.styled_primary(italic)
            ),
            "…and a real Italic",
        );
        assert!(
            !Arc::ptr_eq(
                stack.styled_primary(bold),
                stack.styled_primary(bold_italic)
            ),
            "Bold and BoldItalic are distinct faces",
        );
        assert!(Arc::ptr_eq(
            stack.resolve('A', bold),
            stack.styled_primary(bold)
        ));

        let absent = FaceSpec::named("definitely-not-a-real-family-xyz123");
        let missing = StyleFaces {
            bold: &absent,
            ..StyleFaces::default()
        };
        let mut stack2 = FontStack::with_primary_features(regular.clone(), Vec::new());
        stack2.load_styled(&db, regular_id, &regular, base, &[], &missing);
        assert!(
            Arc::ptr_eq(stack2.styled_primary(bold), &regular),
            "a missing bold override falls back to the regular face",
        );
    }
}

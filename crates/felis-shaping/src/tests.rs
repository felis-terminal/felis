#[cfg(not(windows))]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use proptest::prelude::*;

use super::*;
#[cfg(not(windows))]
use crate::provider::ResolvedFace;

#[cfg(not(windows))]
mod clusters_and_marks;
#[cfg(not(windows))]
mod emoji_presentation;
#[cfg(not(windows))]
mod fallback_styles;
mod families_and_metrics;
mod feature_parsing;
#[cfg(not(windows))]
mod features;
#[cfg(not(windows))]
mod host_faces;
mod italic_position;
#[cfg(not(windows))]
mod metrics_and_cache;
mod missing_fonts;
#[cfg(not(windows))]
mod shape_run;
#[cfg(not(windows))]
mod stack_resolution;
#[cfg(not(windows))]
mod variable_axes;

/// The pinned set behind the provider [`system_provider`] would
/// build, with the `monospace` generic naming the probe family as a
/// host configuration would name its own.
#[cfg(target_os = "linux")]
fn pinned_system() -> provider::fontconfig::Fontconfig {
    provider::fontconfig::pinned_config(
        &format!(
            "<alias><family>monospace</family>\
             <prefer><family>{TEST_FONT_FAMILY}</family></prefer></alias>"
        ),
        "en",
    )
    .expect("libfontconfig loads on Linux")
}

#[cfg(not(target_os = "linux"))]
fn pinned_system() -> FileSet {
    let mut db = FileSet::default();
    db.add_dir(test_font_dir());
    db.set_monospace_family(TEST_FONT_FAMILY);
    db
}

/// [`FontStack::auto_discover`] over the pinned set.
fn discover(
    primary: Option<&str>,
    features: &[String],
    fallbacks: &[FaceSpec],
    styles: &StyleFaces<'_>,
) -> Result<FontStack, ShapingError> {
    FontStack::auto_discover_in(&pinned_system(), primary, features, fallbacks, styles)
}

fn font() -> Font {
    Font::load_test_font()
}

fn stack() -> FontStack {
    discover(None, &[], &[], &StyleFaces::default()).expect("primary must load")
}

/// The family of the pinned CJK subset, which maps U+845B and its
/// ideographic variation sequences alone.
const PINNED_CJK_FAMILY: &str = "Noto Sans CJK JP";

fn test_font_files() -> Vec<std::path::PathBuf> {
    std::fs::read_dir(test_font_dir().join("truetype"))
        .expect("FELIS_TEST_FONT_DIR has a truetype directory")
        .map(|entry| entry.expect("readable directory entry").path())
        .filter(|path| path.to_string_lossy().contains("MonaspaceNeon"))
        .collect()
}

fn design_value(font: &Font, tag: Tag, value: f32) -> F2Dot14 {
    let index = font
        .face
        .axes()
        .iter()
        .position(|axis| axis.tag() == tag)
        .expect("the face has the axis");
    font.normalized_coords(&[(tag, value)])[index]
}

/// The pinned faces named by path under `$FELIS_TEST_FONT_DIR`.
fn pinned_files(names: &[&str]) -> Vec<std::path::PathBuf> {
    let dir = test_font_dir();
    names.iter().map(|name| dir.join(name)).collect()
}

#[cfg(not(windows))]
fn pinned_variable_font() -> Font {
    let mut db = FileSet::default();
    db.add_file(&pinned_files(&["truetype/Monaspace Neon Var.ttf"])[0])
        .expect("the variable face reads");
    Font::try_load_with(&db, "Monaspace Neon Var").expect("the variable face loads")
}

const SYMBOLS2: &str = "NotoSansSymbols2-Regular.otf";

const EMOJI: &str = "NotoColorEmoji.ttf";

fn axis_coord(font: &Font, tag: [u8; 4]) -> F2Dot14 {
    let tag = Tag::new(&tag);
    let index = font
        .face
        .axes()
        .iter()
        .position(|v| v.tag() == tag)
        .expect("the face has the axis");
    font.coords[index]
}

fn ink(font: &Font, c: char) -> u64 {
    rasterize(&mut Rasterizer::default(), font, c, 32.0)
        .pixels()
        .as_bytes()
        .iter()
        .map(|&p| u64::from(p))
        .sum()
}

const BOLD: FontStyle = FontStyle {
    bold: true,
    italic: false,
};

const ITALIC: FontStyle = FontStyle {
    bold: false,
    italic: true,
};

const BOLD_ITALIC: FontStyle = FontStyle {
    bold: true,
    italic: true,
};

fn axis(tag: [u8; 4], default: f32) -> AxisInfo {
    AxisInfo {
        tag: Tag::new(&tag),
        default,
    }
}

fn instance(name: &str, values: &[f32]) -> InstanceInfo {
    InstanceInfo {
        name: name.to_owned(),
        values: values.to_vec(),
    }
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

/// The pinned set with `fallbacks` as an explicit `font.fallback`
/// list, or the discovered chain when it is empty.
fn pinned_stack(fallbacks: &[&str]) -> FontStack {
    let mut db = FileSet::default();
    db.add_dir(test_font_dir());
    let specs: Vec<FaceSpec> = fallbacks.iter().map(|f| FaceSpec::named(*f)).collect();
    FontStack::auto_discover_in(&db, None, &[], &specs, &StyleFaces::default())
        .expect("the pinned set loads")
}

pub(crate) const TRAK_PROBE: &str = "../trak-probe/FelisTrakProbe.ttf";

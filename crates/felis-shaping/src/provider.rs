//! The face-discovery seam: everything `FontStack` asks of the host's font
//! set, so the stack never names a discovery library's types.

use std::{collections::HashMap, hash::Hash, path::PathBuf};

use fontdb::{Database, Family, Query};
use skrifa::{
    MetadataProvider,
    instance::{LocationRef, Size},
};

use crate::{
    FaceStyle, FontBytes, FontStyle, InstalledFamily, ShapingError, named_instance_styles,
};

#[cfg(target_os = "macos")]
pub(crate) mod coretext;
#[cfg(any(target_os = "linux", all(test, unix)))]
pub(crate) mod fontconfig;

pub(crate) const REGULAR_WEIGHT: u16 = 400;
pub(crate) const BOLD_WEIGHT: u16 = 700;

pub(crate) trait FaceProvider {
    type Id: Copy + Eq + Hash;

    /// The face of `family` closest to `weight` and `italic`; `None` when
    /// no face of that family is installed.
    fn find_face(&self, family: &str, weight: u16, italic: bool) -> Option<Self::Id>;

    /// The face of `family`, which `id` was found under, closest to
    /// `weight` and `italic`.
    fn find_sibling(
        &self,
        _id: Self::Id,
        family: &str,
        weight: u16,
        italic: bool,
    ) -> Option<Self::Id> {
        self.find_face(family, weight, italic)
    }

    /// The face drawn when `font.family` is unset or names nothing, and
    /// the family name its styled faces derive from.
    fn default_monospace(&self) -> Result<(Self::Id, String), ShapingError>;

    fn resolve_face(&self, id: Self::Id) -> Option<ResolvedFace>;

    /// The family a styled lookup of `id`'s bold or italic face names.
    fn family_of(&self, _id: Self::Id) -> Option<String> {
        None
    }

    /// The host's fallback ranked for `requested` (whose config rules may
    /// name fallbacks the matched family's name does not), the primary's
    /// own face included; `None` makes the stack discover fallbacks by
    /// family. What loading showed (`loaded`) overrides the provider.
    fn fallback_cascade(
        &self,
        _requested: &str,
        _loaded: &HashMap<Self::Id, LoadedAs>,
    ) -> Option<Vec<CascadeFace<Self::Id>>> {
        None
    }

    /// The host's per-character fallback for a stack whose regular primary
    /// is `primary`, asked for what no ordered face covers; `None` makes
    /// the stack discover fallbacks by family.
    fn system_fallback(&self, _primary: Self::Id) -> Option<Box<dyn SystemFallback>> {
        None
    }
}

/// The face the host draws a scalar with, asked only once the stack's own
/// faces have missed it.
pub(crate) trait SystemFallback: Send + Sync {
    /// The face the host picks for `query` (one scalar, a presentation
    /// selector appended when one was asked for) next to the primary at
    /// `style`; `Ok(None)` when it has none. An `Err` is not remembered,
    /// so the next miss asks again.
    fn face_for(&self, query: &str, style: FontStyle)
    -> Result<Option<ResolvedFace>, ShapingError>;
}

#[derive(Clone, Copy)]
pub(crate) enum LoadedAs {
    #[cfg_attr(
        not(any(target_os = "linux", all(test, not(windows)))),
        expect(dead_code, reason = "only the Linux fontconfig provider reads it")
    )]
    Color(bool),
    /// The loader cannot parse it, so it must not take coverage from a
    /// face ranked after it.
    Unusable,
}

pub(crate) struct CascadeFace<Id> {
    pub id: Id,
    pub color: bool,
}

pub(crate) struct ResolvedFace {
    pub source: FaceSource,
    pub index: u32,
    /// Zero-based named instance in `fvar` order the face is drawn at;
    /// `None` loads the file's default position.
    pub instance: Option<usize>,
    /// The face itself is italic or oblique, so an italic query needs no
    /// axis move to reach it.
    pub slanted: bool,
}

pub(crate) enum FaceSource {
    /// A file under a system font root, mapped by the loader.
    File(PathBuf),
    Shared(FontBytes),
}

/// The face CSS Fonts 4 font matching picks among `faces` for `weight`
/// and `italic`: the slant first, then the weight. `None` for no faces.
pub(crate) fn closest_style(faces: &[FaceStyle], weight: u16, italic: bool) -> Option<usize> {
    let slant_matches = faces.iter().any(|face| face.italic == italic);
    (0..faces.len())
        .filter(|&i| !slant_matches || faces[i].italic == italic)
        .min_by_key(|&i| weight_rank(weight, faces[i].weight))
}

/// Lower ranks first. 400 and 500 look up to 500, then down, then up past
/// 500; a lighter weight looks down first, a heavier one up first.
fn weight_rank(desired: u16, weight: u16) -> (u8, u16) {
    match desired {
        400..=500 if (desired..=500).contains(&weight) => (0, weight - desired),
        400..=500 if weight < desired => (1, desired - weight),
        400..=500 => (2, weight - 500),
        ..400 if weight <= desired => (0, desired - weight),
        ..400 => (1, weight - desired),
        _ if weight >= desired => (0, weight - desired),
        _ => (1, desired - weight),
    }
}

/// The language the platform fallback is asked in: the first of the
/// user's languages that writes Han, else the first. Only Han changes
/// form by language, so a list led by English still decides it.
#[cfg(any(target_os = "macos", windows, test))]
pub(crate) fn fallback_language<S: AsRef<str>>(languages: &[S]) -> Option<&str> {
    const HAN_LANGUAGES: &[&str] = &[
        "ja", "ko", "zh", "cmn", "yue", "wuu", "hak", "nan", "gan", "hsn", "lzh",
    ];
    const HAN_SCRIPTS: &[&str] = &["Hani", "Hans", "Hant", "Hanb", "Jpan", "Kore"];
    let writes_han = |language: &&str| {
        let mut subtags = language.split(['-', '_']);
        let primary = subtags.next().unwrap_or_default();
        HAN_LANGUAGES
            .iter()
            .any(|han| primary.eq_ignore_ascii_case(han))
            || subtags.any(|subtag| {
                HAN_SCRIPTS
                    .iter()
                    .any(|script| subtag.eq_ignore_ascii_case(script))
            })
    };
    languages
        .iter()
        .map(AsRef::as_ref)
        .find(writes_han)
        .or_else(|| languages.first().map(AsRef::as_ref))
}

#[cfg(test)]
mod language_tests {
    use super::fallback_language;

    #[test]
    fn the_fallback_language_is_the_first_that_writes_han() {
        assert_eq!(
            fallback_language(&["en-US", "zh-Hans-CN", "ja"]),
            Some("zh-Hans-CN")
        );
        assert_eq!(fallback_language(&["en-GB", "fr", "ja-JP"]), Some("ja-JP"));
        assert_eq!(fallback_language(&["de", "KO_kr"]), Some("KO_kr"));
        assert_eq!(
            fallback_language(&["yue-Hant-HK", "ja-JP"]),
            Some("yue-Hant-HK")
        );
        assert_eq!(
            fallback_language(&["en", "cmn-Hans", "ja"]),
            Some("cmn-Hans")
        );
        assert_eq!(
            fallback_language(&["fr", "und-Hant", "ja"]),
            Some("und-Hant")
        );
    }

    #[test]
    fn a_list_without_a_han_language_asks_in_its_first() {
        assert_eq!(fallback_language(&["en-US", "fr-FR"]), Some("en-US"));
        assert_eq!(fallback_language(&["jam", "zha"]), Some("jam"));
        assert_eq!(fallback_language::<&str>(&[]), None);
    }
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

/// Rescans every installed font (~300 ms on a large system); share one
/// across probes.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn system_db() -> Database {
    let mut db = Database::new();
    db.load_system_fonts();
    db
}

/// Every family name a face in `db` carries, the set
/// [`FaceProvider::find_face`] accepts. fontdb compares names exactly, so
/// only identical spellings collapse.
#[cfg_attr(
    all(any(target_os = "linux", target_os = "macos"), not(test)),
    expect(
        dead_code,
        reason = "Linux and macOS list families through the platform"
    )
)]
pub(crate) fn database_families(db: &Database) -> Vec<InstalledFamily> {
    let mut families: HashMap<&str, InstalledFamily> = HashMap::new();
    for face in db.faces() {
        let own = FaceStyle {
            italic: face.style != fontdb::Style::Normal,
            weight: face.weight.0,
        };
        let (monospace, mut styles) = db
            .with_face_data(face.id, |data, index| {
                let instances = usize::try_from(index)
                    .ok()
                    .and_then(|index| swash::FontRef::from_index(data, index))
                    .map(named_instance_styles)
                    .unwrap_or_default();
                (ascii_is_fixed_pitch(data, index), instances)
            })
            .unwrap_or_default();
        if styles.is_empty() {
            styles.push(own);
        }
        for (name, _) in &face.families {
            let family = families
                .entry(name)
                .or_insert_with(|| InstalledFamily::new(name.clone()));
            for &style in &styles {
                family.add_face(monospace, style);
            }
        }
    }
    families.into_values().collect()
}

/// Printable ASCII is what sizes the cell, so neither the face's own
/// `post.isFixedPitch` nor fontconfig's `spacing` decides: Monaco and many
/// CJK faces leave the flag unset, and fontconfig calls Monaco
/// proportional for its wider non-ASCII glyphs.
pub(crate) fn ascii_is_fixed_pitch(data: &[u8], index: u32) -> bool {
    let Ok(font) = skrifa::FontRef::from_index(data, index) else {
        return false;
    };
    let charmap = font.charmap();
    let metrics = font.glyph_metrics(Size::unscaled(), LocationRef::default());
    let mut advances = (' '..='~').map(|c| {
        charmap
            .map(c)
            .and_then(|glyph| metrics.advance_width(glyph))
    });
    let Some(Some(first)) = advances.next() else {
        return false;
    };
    advances.all(|advance| advance == Some(first))
}

impl FaceProvider for Database {
    type Id = fontdb::ID;

    fn find_face(&self, family: &str, weight: u16, italic: bool) -> Option<fontdb::ID> {
        self.query(&Query {
            families: &[Family::Name(family)],
            weight: fontdb::Weight(weight),
            style: if italic {
                fontdb::Style::Italic
            } else {
                fontdb::Style::Normal
            },
            ..Query::default()
        })
    }

    /// fontdb keeps only the first `prefer` entry of the last `monospace`
    /// alias it parses, so on Debian the generic names `FreeMono`
    /// (69-unifont.conf) even when only `DejaVu Sans Mono` is installed.
    fn default_monospace(&self) -> Result<(fontdb::ID, String), ShapingError> {
        let generic = self.family_name(&Family::Monospace);
        let resolve = |name: &str| {
            self.find_face(name, REGULAR_WEIGHT, false)
                .map(|id| (id, name.to_owned()))
        };
        if let Some(found) = resolve(generic) {
            return Ok(found);
        }
        let found = MONOSPACE_FALLBACK_FAMILIES
            .iter()
            .find_map(|name| resolve(name))
            .or_else(|| {
                self.faces()
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

    fn resolve_face(&self, id: fontdb::ID) -> Option<ResolvedFace> {
        let face = self.face(id)?;
        let source = match &face.source {
            fontdb::Source::Binary(bytes) | fontdb::Source::SharedFile(_, bytes) => {
                FaceSource::Shared(bytes.clone())
            }
            fontdb::Source::File(path) => FaceSource::File(path.clone()),
        };
        Some(ResolvedFace {
            source,
            index: face.index,
            instance: None,
            slanted: face.style != fontdb::Style::Normal,
        })
    }
}

#[cfg(test)]
mod matching_tests {
    use super::*;

    const fn style(weight: u16, italic: bool) -> FaceStyle {
        FaceStyle { italic, weight }
    }

    fn picked(faces: &[FaceStyle], weight: u16, italic: bool) -> FaceStyle {
        faces[closest_style(faces, weight, italic).expect("a face is picked")]
    }

    #[test]
    fn the_slant_decides_before_the_weight() {
        let faces = [style(400, false), style(700, true)];
        assert_eq!(picked(&faces, 400, true), style(700, true));
        assert_eq!(picked(&faces, 700, false), style(400, false));
    }

    #[test]
    fn a_regular_query_looks_up_to_medium_then_down_then_up() {
        let faces = [style(300, false), style(500, false), style(600, false)];
        assert_eq!(picked(&faces, 400, false), style(500, false));
        let faces = [style(300, false), style(600, false)];
        assert_eq!(picked(&faces, 400, false), style(300, false));
        assert_eq!(picked(&faces, 500, false), style(300, false));
    }

    #[test]
    fn a_light_query_looks_down_first_and_a_heavy_one_up_first() {
        let faces = [style(200, false), style(350, false), style(800, false)];
        assert_eq!(picked(&faces, 300, false), style(200, false));
        assert_eq!(picked(&faces, 700, false), style(800, false));
        assert_eq!(picked(&faces[..2], 700, false), style(350, false));
    }

    #[test]
    fn no_face_picks_nothing() {
        assert_eq!(closest_style(&[], 400, false), None);
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;
    use crate::{TEST_FONT_FAMILY, test_font_dir};

    #[test]
    fn the_database_lists_each_family_once() {
        let mut db = Database::new();
        db.load_fonts_dir(test_font_dir());
        let mut names: Vec<String> = database_families(&db)
            .into_iter()
            .map(|family| family.name)
            .collect();
        assert!(names.iter().any(|n| n == TEST_FONT_FAMILY), "{names:?}");
        for name in &names {
            assert!(
                db.find_face(name, REGULAR_WEIGHT, false).is_some(),
                "{name} is listed but not accepted"
            );
        }
        names.sort_unstable();
        assert!(names.windows(2).all(|w| w[0] != w[1]), "{names:?}");
    }

    #[test]
    fn the_database_reports_each_familys_pitch_and_styles() {
        let mut db = Database::new();
        db.load_fonts_dir(test_font_dir());
        let families = database_families(&db);
        let named = |name: &str| {
            families
                .iter()
                .find(|family| family.name == name)
                .unwrap_or_else(|| panic!("{name} is installed"))
        };
        let argon = named("Monaspace Argon");
        assert!(argon.monospace);
        for style in [
            (false, 400),
            (false, BOLD_WEIGHT),
            (true, 400),
            (true, BOLD_WEIGHT),
        ] {
            let style = FaceStyle {
                italic: style.0,
                weight: style.1,
            };
            assert!(argon.styles.contains(&style), "{:?}", argon.styles);
        }
        assert!(argon.styles.is_sorted(), "{:?}", argon.styles);
        assert!(!named("Noto Sans Symbols").monospace);
        let variable = &named("Monaspace Neon Var").styles;
        for (italic, weight) in [(false, 400), (false, 700), (true, 700)] {
            let style = FaceStyle { italic, weight };
            assert!(variable.contains(&style), "named instances: {variable:?}");
        }
        assert!(
            !named("Noto Color Emoji").monospace,
            "printable ASCII is not mapped"
        );
    }
}

//! The face-discovery seam: everything `FontStack` asks of the host's font
//! set, so the stack never names a discovery library's types.

use std::{collections::HashMap, hash::Hash, path::PathBuf};

use fontdb::{Database, Family, Query};
use skrifa::{
    MetadataProvider,
    instance::{LocationRef, Size},
};

use crate::{FaceStyle, FontBytes, InstalledFamily, ShapingError, named_instance_styles};

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
#[cfg(not(target_os = "linux"))]
pub(crate) fn system_db() -> Database {
    let mut db = Database::new();
    db.load_system_fonts();
    db
}

/// Every family name a face in `db` carries, the set
/// [`FaceProvider::find_face`] accepts. fontdb compares names exactly, so
/// only identical spellings collapse.
#[cfg_attr(
    all(target_os = "linux", not(test)),
    expect(dead_code, reason = "Linux lists families through fontconfig")
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

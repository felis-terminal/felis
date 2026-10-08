//! The face-discovery seam: everything `FontStack` asks of the host's font
//! set, so the stack never names a discovery library's types.

use std::{hash::Hash, path::PathBuf};

use fontdb::{Database, Family, Query};

use crate::{FontBytes, ShapingError};

pub(crate) const REGULAR_WEIGHT: u16 = 400;
pub(crate) const BOLD_WEIGHT: u16 = 700;

pub(crate) trait FaceProvider {
    type Id: Copy + Eq + Hash;

    /// The face of `family` closest to `weight` and `italic`; `None` when
    /// no face of that family is installed.
    fn find_face(&self, family: &str, weight: u16, italic: bool) -> Option<Self::Id>;

    /// The face drawn when `font.family` is unset or names nothing, and
    /// the family name its styled faces derive from.
    fn default_monospace(&self) -> Result<(Self::Id, String), ShapingError>;

    fn resolve_face(&self, id: Self::Id) -> Option<ResolvedFace>;
}

pub(crate) struct ResolvedFace {
    pub source: FaceSource,
    pub index: u32,
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
pub(crate) fn system_db() -> Database {
    let mut db = Database::new();
    db.load_system_fonts();
    db
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
            slanted: face.style != fontdb::Style::Normal,
        })
    }
}

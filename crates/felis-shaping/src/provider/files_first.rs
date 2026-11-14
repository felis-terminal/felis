//! A caller's font files searched before the system's fonts: each family
//! name resolves in the files when they carry it, else in the system, and
//! the automatic fallback stays the system's.

use std::collections::HashMap;

use super::{
    CascadeFace, FaceProvider, LoadedAs, REGULAR_WEIGHT, ResolvedFace, SystemFallback,
    files::FileSet,
};
use crate::{NERD_FONT_FAMILIES, ShapingError};

pub(crate) struct FilesFirst<S> {
    pub(crate) files: FileSet,
    pub(crate) system: S,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Layered<I> {
    File(usize),
    System(I),
}

impl<S: FaceProvider> FilesFirst<S> {
    fn in_files(&self, family: &str) -> bool {
        self.files
            .find_face(family, REGULAR_WEIGHT, false)
            .is_some()
    }

    /// The files' first face of `candidates`, ahead of the system's own
    /// rank for those families.
    fn files_cascade_head(&self, candidates: &[&str]) -> Option<CascadeFace<Layered<S::Id>>> {
        candidates.iter().find_map(|family| {
            let id = self.files.find_face(family, REGULAR_WEIGHT, false)?;
            Some(CascadeFace {
                id: Layered::File(id),
                color: false,
            })
        })
    }
}

impl<S: FaceProvider> FaceProvider for FilesFirst<S> {
    type Id = Layered<S::Id>;

    /// Per family, not per face: a family the files carry draws every
    /// style from them, since a same-named system family may be another
    /// version whose advances differ.
    fn find_face(&self, family: &str, weight: u16, italic: bool) -> Option<Self::Id> {
        self.files
            .find_face(family, weight, italic)
            .map(Layered::File)
            .or_else(|| {
                self.system
                    .find_face(family, weight, italic)
                    .map(Layered::System)
            })
    }

    fn find_sibling(
        &self,
        id: Self::Id,
        family: &str,
        weight: u16,
        italic: bool,
    ) -> Option<Self::Id> {
        match id {
            Layered::File(_) => self.find_face(family, weight, italic),
            Layered::System(id) => self
                .system
                .find_sibling(id, family, weight, italic)
                .map(Layered::System),
        }
    }

    fn default_monospace(&self, weight: u16) -> Result<(Self::Id, String), ShapingError> {
        if let Ok((id, family)) = self.files.default_monospace(weight)
            && self.files.is_fixed_pitch(id)
        {
            return Ok((Layered::File(id), family));
        }
        let (id, family) = self.system.default_monospace(weight)?;
        Ok((Layered::System(id), family))
    }

    fn resolve_face(&self, id: Self::Id) -> Option<ResolvedFace> {
        match id {
            Layered::File(id) => self.files.resolve_face(id),
            Layered::System(id) => self.system.resolve_face(id),
        }
    }

    fn family_of(&self, id: Self::Id) -> Option<String> {
        match id {
            Layered::File(id) => self.files.family_of(id),
            Layered::System(id) => self.system.family_of(id),
        }
    }

    /// A family only the files carry is unknown to the host, whose rules
    /// would rank it as their default text family, so the cascade is the
    /// one ranked for `monospace`.
    fn fallback_cascade(
        &self,
        requested: &str,
        loaded: &HashMap<Self::Id, LoadedAs>,
    ) -> Option<Vec<CascadeFace<Self::Id>>> {
        let requested = if self.in_files(requested) {
            "monospace"
        } else {
            requested
        };
        let system_loaded = loaded
            .iter()
            .filter_map(|(&id, &verdict)| match id {
                Layered::System(id) => Some((id, verdict)),
                Layered::File(_) => None,
            })
            .collect();
        let system = self.system.fallback_cascade(requested, &system_loaded)?;
        let head = self
            .files_cascade_head(NERD_FONT_FAMILIES)
            .and_then(|face| match loaded.get(&face.id) {
                Some(LoadedAs::Unusable) => None,
                Some(&LoadedAs::Color(color)) => Some(CascadeFace { color, ..face }),
                None => Some(face),
            });
        Some(
            head.into_iter()
                .chain(system.into_iter().map(|face| CascadeFace {
                    id: Layered::System(face.id),
                    color: face.color,
                }))
                .collect(),
        )
    }

    fn system_fallback(&self, primary: Option<Self::Id>) -> Option<Box<dyn SystemFallback>> {
        match primary {
            Some(Layered::System(id)) => self.system.system_fallback(Some(id)),
            Some(Layered::File(_)) | None => self.system.system_fallback(None),
        }
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn a_candidate_family_in_the_files_heads_the_cascade() {
        let mut files = FileSet::default();
        files
            .add_file(crate::test_font_dir().join("opentype/MonaspaceArgonNF-Regular.otf"))
            .expect("the pinned file reads");
        let db = FilesFirst {
            files,
            system: FileSet::default(),
        };
        let head = db.files_cascade_head(&["Symbols Nerd Font", "Monaspace Argon NF"]);
        assert!(matches!(
            head,
            Some(CascadeFace {
                id: Layered::File(_),
                color: false
            })
        ));
        assert!(db.files_cascade_head(&["Symbols Nerd Font"]).is_none());
    }
}

//! Faces from a given set of font files, matched by the family names their
//! `name` tables carry, so the same files resolve to the same faces on
//! every machine.

use std::{path::Path, sync::Arc};

use swash::{FontDataRef, FontRef, Stretch, StringId, Style};

use super::{
    FaceProvider, FaceSource, MONOSPACE_FALLBACK_FAMILIES, ResolvedFace, ascii_is_fixed_pitch,
    closest_style,
};
use crate::{FaceStyle, FontBytes, ShapingError};

struct Face {
    bytes: FontBytes,
    index: u32,
    /// Typographic family names when the face has any, else its legacy
    /// ones; English first.
    families: Vec<String>,
    style: FaceStyle,
    /// Distance from the normal width, in half-percent steps.
    off_width: u16,
    monospace: bool,
}

#[derive(Default)]
pub(crate) struct FileSet {
    faces: Vec<Face>,
    /// Takes the place of the platform's monospace default.
    monospace: Option<String>,
}

impl FileSet {
    /// Every face in `bytes`; `false` when it holds none.
    pub(crate) fn add(&mut self, bytes: Vec<u8>) -> bool {
        let bytes: FontBytes = Arc::new(bytes);
        let before = self.faces.len();
        let Some(file) = FontDataRef::new(bytes.as_ref().as_ref()) else {
            return false;
        };
        let found: Vec<Face> = file
            .fonts()
            .enumerate()
            .filter_map(|(index, font)| {
                let index = u32::try_from(index).ok()?;
                let attributes = font.attributes();
                Some(Face {
                    bytes: bytes.clone(),
                    index,
                    families: family_names(font),
                    style: FaceStyle {
                        italic: attributes.style() != Style::Normal,
                        weight: attributes.weight().0,
                    },
                    off_width: attributes.stretch().raw().abs_diff(Stretch::NORMAL.raw()),
                    monospace: ascii_is_fixed_pitch(bytes.as_ref().as_ref(), index),
                })
            })
            .collect();
        self.faces.extend(found);
        self.faces.len() > before
    }

    /// Every font file under `dir`, recursively; unreadable ones are
    /// skipped.
    pub(crate) fn add_dir(&mut self, dir: impl AsRef<Path>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<_> = entries
            .filter_map(|entry| Some(entry.ok()?.path()))
            .collect();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                self.add_dir(&path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                self.add(bytes);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn add_file(&mut self, path: impl AsRef<Path>) -> std::io::Result<()> {
        if self.add(std::fs::read(path)?) {
            Ok(())
        } else {
            Err(std::io::Error::other("no font face in the file"))
        }
    }

    #[cfg(test)]
    pub(crate) fn set_monospace_family(&mut self, family: &str) {
        self.monospace = Some(family.to_owned());
    }

    /// The style and fixed pitch the set recorded for `id`.
    #[cfg(all(test, not(windows)))]
    pub(crate) fn traits_of(&self, id: usize) -> Option<(FaceStyle, bool)> {
        let face = self.faces.get(id)?;
        Some((face.style, face.monospace))
    }

    /// The family's faces nearest the normal width: a family that ships
    /// wide and semi-wide statics under one name is asked for its normal
    /// width, as the platform databases ask for it.
    fn named(&self, family: &str) -> Vec<usize> {
        let ids: Vec<usize> = (0..self.faces.len())
            .filter(|&id| self.faces[id].families.iter().any(|name| name == family))
            .collect();
        let nearest = ids.iter().map(|&id| self.faces[id].off_width).min();
        ids.into_iter()
            .filter(|&id| Some(self.faces[id].off_width) == nearest)
            .collect()
    }
}

impl FaceProvider for FileSet {
    type Id = usize;

    fn find_face(&self, family: &str, weight: u16, italic: bool) -> Option<usize> {
        let ids = self.named(family);
        let styles: Vec<FaceStyle> = ids.iter().map(|&id| self.faces[id].style).collect();
        closest_style(&styles, weight, italic).map(|at| ids[at])
    }

    /// The set's own default when it names one, else the families every
    /// platform tries, else its first fixed-pitch family by name.
    fn default_monospace(&self, weight: u16) -> Result<(usize, String), ShapingError> {
        let mut fixed_pitch: Vec<&str> = self
            .faces
            .iter()
            .filter(|face| face.monospace)
            .filter_map(|face| face.families.first().map(String::as_str))
            .collect();
        fixed_pitch.sort_unstable();
        self.monospace
            .as_deref()
            .into_iter()
            .chain(MONOSPACE_FALLBACK_FAMILIES.iter().copied())
            .chain(fixed_pitch)
            .find_map(|family| {
                let id = self.find_face(family, weight, false)?;
                Some((id, family.to_owned()))
            })
            .ok_or(ShapingError::NoFont)
    }

    fn resolve_face(&self, id: usize) -> Option<ResolvedFace> {
        let face = self.faces.get(id)?;
        Some(ResolvedFace {
            source: FaceSource::Shared(face.bytes.clone()),
            index: face.index,
            instance: None,
            slanted: face.style.italic,
        })
    }

    fn family_of(&self, id: usize) -> Option<String> {
        self.faces.get(id)?.families.first().cloned()
    }
}

/// English names first, as a styled lookup of the face asks for them.
fn family_names(font: FontRef<'_>) -> Vec<String> {
    let strings = font.localized_strings();
    let typographic = strings
        .clone()
        .any(|name| name.id() == StringId::TypographicFamily);
    let id = if typographic {
        StringId::TypographicFamily
    } else {
        StringId::Family
    };
    let mut names: Vec<(bool, String)> = strings
        .filter(|name| name.id() == id)
        .map(|name| (!name.language().starts_with("en"), name.to_string()))
        .filter(|(_, name)| !name.is_empty())
        .collect();
    names.sort_by_key(|(foreign, _)| *foreign);
    let mut unique = Vec::new();
    for (_, name) in names {
        if !unique.contains(&name) {
            unique.push(name);
        }
    }
    unique
}

//! What a face says about itself: its names, `OS/2` style, axes and named
//! instances, read through the font model.

use skrifa::{
    Tag,
    font::name::{Localized, Name},
    raw::{
        FileRef, FontRef, TableProvider,
        model::Blob,
        types::{F2Dot14, Fixed, NameId},
    },
};

use crate::FontBytes;

/// The face at `index` of `bytes`, every table read in place.
pub(crate) fn model_face(bytes: &FontBytes, index: u32) -> Option<skrifa::Font> {
    skrifa::Font::new(Blob::Shared(bytes.clone()), index)
}

/// How many faces `data` holds: one for a single font, its length for a
/// collection, none when it parses as neither.
pub(crate) fn face_count(data: &[u8]) -> u32 {
    match FileRef::new(data) {
        Ok(FileRef::Font(_)) => 1,
        Ok(FileRef::Collection(collection)) => collection.len(),
        Err(_) => 0,
    }
}

/// The record a lookup by `language` settles on: the first whose BCP 47
/// tag starts with it in a Unicode encoding, else the last that matches in
/// any encoding, else, for `None`, the first record of the name.
pub(crate) fn pick<'a>(name: &Name<'a>, language: Option<&str>) -> Option<Localized<'a>> {
    let (mut first, mut matched) = (None, None);
    for localized in name.iter() {
        first.get_or_insert(localized);
        let hit = match language {
            Some(prefix) => localized
                .language()
                .is_some_and(|tag| tag.starts_with(prefix)),
            None => localized.language().is_none(),
        };
        if hit {
            if is_unicode(localized) {
                return Some(localized);
            }
            matched = Some(localized);
        }
    }
    matched.or_else(|| if language.is_none() { first } else { None })
}

fn is_unicode(localized: Localized<'_>) -> bool {
    localized.sfnt_record().is_some_and(|record| {
        matches!(
            (record.platform_id(), record.encoding_id()),
            (0, _) | (3, 0 | 1 | 10)
        )
    })
}

/// The string `id` names, English first.
pub(crate) fn english_name(face: &skrifa::Font, id: NameId) -> Option<String> {
    let name = face.names().get(id)?;
    pick(&name, Some("en"))
        .or_else(|| pick(&name, None))
        .map(|found| found.name().to_string())
}

/// The string `id` names in the record a language-free lookup picks.
#[cfg(target_os = "macos")]
pub(crate) fn any_name(face: &skrifa::Font, id: NameId) -> Option<String> {
    let name = face.names().get(id)?;
    pick(&name, None).map(|found| found.name().to_string())
}

/// Typographic family names when the face has any, else its legacy ones;
/// English first, without repeats.
pub(crate) fn family_names(face: &skrifa::Font) -> Vec<String> {
    let names = face.names();
    let Some(name) = names
        .get(NameId::TYPOGRAPHIC_FAMILY_NAME)
        .or_else(|| names.get(NameId::FAMILY_NAME))
    else {
        return Vec::new();
    };
    let mut found: Vec<(bool, String)> = name
        .iter()
        .map(|localized| {
            let english = localized
                .language()
                .is_some_and(|tag| tag.starts_with("en"));
            (!english, localized.name().to_string())
        })
        .filter(|(_, name)| !name.is_empty())
        .collect();
    found.sort_by_key(|(foreign, _)| *foreign);
    let mut unique = Vec::new();
    for (_, name) in found {
        if !unique.contains(&name) {
            unique.push(name);
        }
    }
    unique
}

/// Every legacy and typographic family name, in every language.
#[cfg(target_os = "macos")]
pub(crate) fn all_family_names(face: &skrifa::Font) -> Vec<String> {
    let names = face.names();
    [NameId::FAMILY_NAME, NameId::TYPOGRAPHIC_FAMILY_NAME]
        .into_iter()
        .filter_map(|id| names.get(id))
        .flat_map(|name| {
            name.iter()
                .map(|localized| localized.name().to_string())
                .collect::<Vec<_>>()
        })
        .filter(|name| !name.is_empty())
        .collect()
}

/// A face's declared style, from `OS/2`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Attributes {
    pub(crate) weight: u16,
    pub(crate) italic: bool,
    /// Distance from the normal width, in half-percent steps.
    pub(crate) off_width: u16,
}

/// Without `OS/2`, a face is a regular upright of normal width.
pub(crate) fn attributes(data: &[u8], index: u32) -> Attributes {
    let os2 = FontRef::from_index(data, index).and_then(|font| font.os2());
    let Ok(os2) = os2 else {
        return Attributes {
            weight: 400,
            italic: false,
            off_width: 0,
        };
    };
    let selection = os2.fs_selection().bits();
    let half_percent: u16 = match os2.us_width_class() {
        1 => 0,
        2 => 25,
        3 => 50,
        4 => 75,
        6 => 125,
        7 => 150,
        8 => 200,
        9 => 300,
        _ => 100,
    };
    Attributes {
        weight: os2.us_weight_class() & 0x3FF,
        italic: selection & 1 != 0 || selection & (1 << 9) != 0,
        off_width: half_percent.abs_diff(100),
    }
}

/// `settings` in design units normalized through `avar`, one entry per
/// `fvar` axis with unnamed axes at their default; empty for a static
/// face.
pub(crate) fn normalized_coords(data: &[u8], index: u32, settings: &[(Tag, f32)]) -> Vec<F2Dot14> {
    let Ok(font) = FontRef::from_index(data, index) else {
        return Vec::new();
    };
    let Ok(fvar) = font.fvar() else {
        return Vec::new();
    };
    let axes = fvar.axes().map_or(0, <[_]>::len);
    let mut coords = vec![F2Dot14::ZERO; axes];
    let avar = font.avar().ok();
    fvar.user_to_normalized(
        avar.as_ref(),
        settings
            .iter()
            .map(|&(tag, value)| (tag, Fixed::from_f64(f64::from(value)))),
        &mut coords,
    );
    coords
}

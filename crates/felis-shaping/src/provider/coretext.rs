//! Face discovery through CoreText: its family matching, the user's
//! fixed-pitch font and its per-character fallback, so the faces drawn are
//! the ones the platform picks.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    ptr,
    sync::{Mutex, PoisonError},
};

use objc2_core_foundation::{
    CFArray, CFDictionary, CFLocale, CFRange, CFRetained, CFString, CFURL,
};
use objc2_core_text::{
    CTFont, CTFontDescriptor, CTFontManagerCopyAvailableFontFamilyNames, CTFontSymbolicTraits,
    CTFontUIFontType, kCTFontFamilyNameAttribute, kCTFontNameAttribute, kCTFontStyleNameAttribute,
    kCTFontURLAttribute,
};
use swash::{FontDataRef, StringId, Style};
use tracing::debug;

use super::{
    FaceProvider, FaceSource, MONOSPACE_FALLBACK_FAMILIES, REGULAR_WEIGHT, ResolvedFace,
    SystemFallback, ascii_is_fixed_pitch, closest_style, fallback_language,
};
use crate::{
    FaceStyle, FontStyle, InstalledFamily, ShapingError, WGHT, has_italic_token, map_face_file,
};

/// The face CoreText answers with when nothing covers a scalar; its glyphs
/// name the block rather than draw the character.
const LAST_RESORT: &str = "LastResort";

/// One face, or one named instance of a variable face, in a font file.
#[derive(Clone)]
struct FileFace {
    post_script: String,
    /// The instance's subfamily name; `None` for the face itself.
    instance_name: Option<String>,
    index: u32,
    instance: Option<usize>,
    style: FaceStyle,
}

#[derive(Clone)]
struct Face {
    path: PathBuf,
    family: String,
    post_script: String,
    index: u32,
    instance: Option<usize>,
    style: FaceStyle,
}

/// A face by file, collection index and named instance.
type FaceKey = (PathBuf, u32, Option<usize>);

/// What a CoreText font descriptor says about the face it names.
struct Described {
    path: PathBuf,
    post_script: String,
    style_name: Option<String>,
    family: String,
}

pub(crate) struct CoreText {
    faces: RefCell<Vec<Face>>,
    ids: RefCell<HashMap<FaceKey, usize>>,
    files: RefCell<HashMap<PathBuf, Vec<FileFace>>>,
}

impl CoreText {
    pub(crate) fn system() -> Self {
        Self {
            faces: RefCell::default(),
            ids: RefCell::default(),
            files: RefCell::default(),
        }
    }

    /// The faces CoreText matches to `family`, a localized name included;
    /// empty for a name it does not know, which it never substitutes.
    fn family_faces(&self, family: &str) -> Vec<usize> {
        let name = CFString::from_str(family);
        // SAFETY: reading one of CoreText's immutable attribute-name
        // constants.
        let key = unsafe { kCTFontFamilyNameAttribute };
        let attributes = CFDictionary::<CFString, CFString>::from_slices(&[key], &[&name]);
        // SAFETY: the dictionary maps the family-name attribute to the
        // CFString value CoreText documents for it.
        let descriptor = unsafe { CTFontDescriptor::with_attributes(attributes.as_ref()) };
        // SAFETY: `None` asks for no mandatory attributes.
        let Some(matched) = (unsafe { descriptor.matching_font_descriptors(None) }) else {
            return Vec::new();
        };
        // SAFETY: CoreText documents the array as holding font descriptors.
        let matched = unsafe { CFRetained::cast_unchecked::<CFArray<CTFontDescriptor>>(matched) };
        matched
            .iter()
            .filter_map(|descriptor| self.record(&descriptor))
            .collect()
    }

    fn record(&self, descriptor: &CTFontDescriptor) -> Option<usize> {
        let described = describe(descriptor)?;
        let face = {
            let mut files = self.files.borrow_mut();
            let in_file = files
                .entry(described.path.clone())
                .or_insert_with(|| file_faces(&described.path));
            locate(in_file, &described)?.clone()
        };
        let key = (described.path.clone(), face.index, face.instance);
        let mut faces = self.faces.borrow_mut();
        Some(*self.ids.borrow_mut().entry(key).or_insert_with(|| {
            faces.push(Face {
                path: described.path,
                family: described.family,
                post_script: face.post_script,
                index: face.index,
                instance: face.instance,
                style: face.style,
            });
            faces.len() - 1
        }))
    }

    fn face(&self, id: usize) -> Option<Face> {
        self.faces.borrow().get(id).cloned()
    }

    /// Every family CoreText lists, plus each localized name of their
    /// faces that it also matches, so the list holds the names
    /// [`FaceProvider::find_face`] accepts.
    pub(crate) fn installed_families(&self) -> Vec<InstalledFamily> {
        // SAFETY: the call has no preconditions.
        let names = unsafe { CTFontManagerCopyAvailableFontFamilyNames() };
        // SAFETY: CoreText documents the array as holding family names.
        let names = unsafe { CFRetained::cast_unchecked::<CFArray<CFString>>(names) };
        let mut pitches = HashMap::new();
        let mut listed = HashSet::new();
        let mut aliases = Vec::new();
        let mut families = Vec::new();
        for name in names.iter() {
            let name = name.to_string();
            let ids = self.family_faces(&name);
            if let Some(family) = self.describe_family(&name, &ids, &mut pitches) {
                aliases.extend(
                    ids.iter()
                        .filter_map(|&id| self.face(id))
                        .flat_map(|face| family_names(&face.path, face.index)),
                );
                listed.insert(name);
                families.push(family);
            }
        }
        for alias in aliases {
            if listed.insert(alias.clone()) {
                let ids = self.family_faces(&alias);
                families.extend(self.describe_family(&alias, &ids, &mut pitches));
            }
        }
        families
    }

    fn describe_family(
        &self,
        name: &str,
        ids: &[usize],
        pitches: &mut HashMap<(PathBuf, u32), bool>,
    ) -> Option<InstalledFamily> {
        if ids.is_empty() {
            return None;
        }
        let mut family = InstalledFamily::new(name.to_owned());
        for face in ids.iter().filter_map(|&id| self.face(id)) {
            let monospace = *pitches
                .entry((face.path.clone(), face.index))
                .or_insert_with(|| {
                    map_face_file(&face.path)
                        .is_ok_and(|data| ascii_is_fixed_pitch(&data, face.index))
                });
            family.add_face(monospace, face.style);
        }
        Some(family)
    }
}

impl FaceProvider for CoreText {
    type Id = usize;

    fn find_face(&self, family: &str, weight: u16, italic: bool) -> Option<usize> {
        let ids = self.family_faces(family);
        let styles: Vec<FaceStyle> = ids
            .iter()
            .filter_map(|&id| self.face(id).map(|face| face.style))
            .collect();
        closest_style(&styles, weight, italic).map(|at| ids[at])
    }

    /// The user's fixed-pitch font, Menlo unless they changed it.
    fn default_monospace(&self) -> Result<(usize, String), ShapingError> {
        // SAFETY: `None` asks for the user's own language.
        let font = unsafe {
            CTFont::new_ui_font_for_language(CTFontUIFontType::UserFixedPitch, 0.0, None)
        };
        // SAFETY: `font` is a live CTFont.
        let user = font.map(|font| unsafe { font.family_name() }.to_string());
        user.iter()
            .map(String::as_str)
            .chain(MONOSPACE_FALLBACK_FAMILIES.iter().copied())
            .find_map(|family| {
                let id = self.find_face(family, REGULAR_WEIGHT, false)?;
                Some((id, family.to_owned()))
            })
            .ok_or(ShapingError::NoFont)
    }

    fn resolve_face(&self, id: usize) -> Option<ResolvedFace> {
        let face = self.face(id)?;
        Some(ResolvedFace {
            source: FaceSource::File(face.path),
            index: face.index,
            instance: face.instance,
            slanted: face.style.italic,
        })
    }

    fn family_of(&self, id: usize) -> Option<String> {
        self.face(id).map(|face| face.family)
    }

    fn system_fallback(&self, primary: usize) -> Option<Box<dyn SystemFallback>> {
        let primary = self.face(primary)?.post_script;
        Some(Box::new(Fallback::new(primary)))
    }
}

/// `CTFontCreateForStringWithLanguage` against the primary, in the
/// language `fallback_language` picks from the user's list.
struct Fallback {
    primary: String,
    language: Option<String>,
    files: Mutex<HashMap<PathBuf, Vec<FileFace>>>,
}

impl Fallback {
    fn new(primary: String) -> Self {
        let language = CFLocale::preferred_languages().and_then(|languages| {
            // SAFETY: CoreFoundation documents the array as holding
            // language identifiers.
            let languages = unsafe { CFRetained::cast_unchecked::<CFArray<CFString>>(languages) };
            let languages: Vec<String> = languages
                .iter()
                .map(|language| language.to_string())
                .collect();
            fallback_language(&languages).map(str::to_owned)
        });
        let fallback = Self {
            primary,
            language,
            files: Mutex::default(),
        };
        // CoreText's first fallback query in a process takes milliseconds;
        // paying it here keeps it off the first frame that needs one.
        drop(fallback.face_for("\u{3042}", FontStyle::REGULAR));
        fallback
    }
}

impl SystemFallback for Fallback {
    fn name(&self) -> &'static str {
        "CoreText"
    }

    fn face_for(
        &self,
        query: &str,
        style: FontStyle,
    ) -> Result<Option<ResolvedFace>, ShapingError> {
        let name = CFString::from_str(&self.primary);
        // SAFETY: a null matrix asks for the identity transform.
        let mut base = unsafe { CTFont::with_name(&name, 0.0, ptr::null()) };
        let mut traits = CTFontSymbolicTraits::empty();
        if style.bold {
            traits |= CTFontSymbolicTraits::TraitBold;
        }
        if style.italic {
            traits |= CTFontSymbolicTraits::TraitItalic;
        }
        if !traits.is_empty() {
            // SAFETY: a null matrix keeps `base`'s transform.
            let styled =
                unsafe { base.copy_with_symbolic_traits(0.0, ptr::null(), traits, traits) };
            base = styled.unwrap_or(base);
        }
        // SAFETY: `base` is a live CTFont.
        let base_name = unsafe { base.post_script_name() }.to_string();
        let text = CFString::from_str(query);
        let language = self.language.as_deref().map(CFString::from_str);
        // SAFETY: the range spans `text`.
        let found = unsafe {
            base.for_string_with_language(
                &text,
                CFRange::new(0, text.length()),
                language.as_deref(),
            )
        };
        // SAFETY: `found` is a live CTFont.
        let descriptor = unsafe { found.font_descriptor() };
        let Some(described) = describe(&descriptor) else {
            debug!(
                query,
                "the system fallback face has no file; drawing .notdef"
            );
            return Ok(None);
        };
        if described.post_script == LAST_RESORT || described.post_script == base_name {
            return Ok(None);
        }
        let mut files = self.files.lock().unwrap_or_else(PoisonError::into_inner);
        let in_file = files
            .entry(described.path.clone())
            .or_insert_with(|| file_faces(&described.path));
        Ok(locate(in_file, &described).map(|face| ResolvedFace {
            source: FaceSource::File(described.path.clone()),
            index: face.index,
            instance: face.instance,
            slanted: face.style.italic,
        }))
    }
}

/// `None` for a descriptor without a file, as CoreText's hidden system
/// faces are.
fn describe(descriptor: &CTFontDescriptor) -> Option<Described> {
    // SAFETY: reading CoreText's immutable attribute-name constants.
    let (url, name, style_name, family) = unsafe {
        (
            kCTFontURLAttribute,
            kCTFontNameAttribute,
            kCTFontStyleNameAttribute,
            kCTFontFamilyNameAttribute,
        )
    };
    // SAFETY: CoreText stores the URL attribute as a CFURL.
    let url = unsafe { descriptor.attribute(url) }?
        .downcast::<CFURL>()
        .ok()?;
    Some(Described {
        path: url.to_file_path()?,
        post_script: string_attribute(descriptor, name)?,
        style_name: string_attribute(descriptor, style_name),
        family: string_attribute(descriptor, family).unwrap_or_default(),
    })
}

fn string_attribute(descriptor: &CTFontDescriptor, key: &CFString) -> Option<String> {
    // SAFETY: `key` is a CoreText attribute name; a value of another type
    // fails the downcast.
    let value = unsafe { descriptor.attribute(key) }?;
    Some(value.downcast::<CFString>().ok()?.to_string())
}

/// The face in `in_file` that `described` names: by PostScript name, else,
/// for a named instance whose name CoreText generates (`Skia-Regular_Bold`),
/// by its style name when exactly one instance carries it.
fn locate<'a>(in_file: &'a [FileFace], described: &Described) -> Option<&'a FileFace> {
    if let Some(face) = in_file
        .iter()
        .find(|face| face.post_script == described.post_script)
    {
        return Some(face);
    }
    let style_name = described.style_name.as_deref()?;
    let mut named = in_file
        .iter()
        .filter(|face| face.instance_name.as_deref() == Some(style_name));
    let face = named.next()?;
    named.next().is_none().then_some(face)
}

fn file_faces(path: &Path) -> Vec<FileFace> {
    let Ok(data) = map_face_file(path) else {
        return Vec::new();
    };
    let Some(file) = FontDataRef::new(&data) else {
        return Vec::new();
    };
    let mut faces = Vec::new();
    for (index, font) in file.fonts().enumerate() {
        let Ok(index) = u32::try_from(index) else {
            break;
        };
        let attributes = font.attributes();
        let weight = attributes.weight().0;
        faces.push(FileFace {
            post_script: font
                .localized_strings()
                .find_by_id(StringId::PostScript, None)
                .map(|name| name.to_string())
                .unwrap_or_default(),
            instance_name: None,
            index,
            instance: None,
            style: FaceStyle {
                italic: attributes.style() != Style::Normal,
                weight,
            },
        });
        let wght = font.variations().position(|axis| axis.tag() == WGHT);
        for (at, instance) in font.instances().enumerate() {
            let name = instance
                .name(Some("en"))
                .or_else(|| instance.name(None))
                .map(|name| name.to_string());
            let instance_weight = wght
                .and_then(|axis| instance.values().nth(axis))
                // Saturates into u16; `wght` is 1–1000 by spec.
                .map_or(weight, |value| {
                    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let rounded = value.round() as u16;
                    rounded
                });
            faces.push(FileFace {
                post_script: instance
                    .postscript_name(None)
                    .map(|name| name.to_string())
                    .unwrap_or_default(),
                style: FaceStyle {
                    italic: name.as_deref().is_some_and(has_italic_token),
                    weight: instance_weight,
                },
                instance_name: name,
                index,
                instance: Some(at),
            });
        }
    }
    faces
}

/// Every family name, in every language, the face's `name` table carries.
fn family_names(path: &Path, index: u32) -> Vec<String> {
    let Ok(data) = map_face_file(path) else {
        return Vec::new();
    };
    let Some(font) = usize::try_from(index)
        .ok()
        .and_then(|index| swash::FontRef::from_index(&data, index))
    else {
        return Vec::new();
    };
    font.localized_strings()
        .filter(|name| matches!(name.id(), StringId::Family | StringId::TypographicFamily))
        .map(|name| name.to_string())
        .filter(|name| !name.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BOLD_WEIGHT, FontStack, StyleFaces};

    #[test]
    fn a_listed_family_resolves_and_an_unknown_one_does_not() {
        let ct = CoreText::system();
        let menlo = ct
            .find_face("Menlo", REGULAR_WEIGHT, false)
            .expect("Menlo ships with macOS");
        let face = ct.resolve_face(menlo).expect("the face resolves");
        assert!(matches!(face.source, FaceSource::File(ref path) if path.exists()));
        assert!(
            ct.find_face("felis no such family", REGULAR_WEIGHT, false)
                .is_none()
        );
    }

    #[test]
    fn a_styled_lookup_picks_the_familys_styled_face() {
        let ct = CoreText::system();
        let regular = ct
            .find_face("Menlo", REGULAR_WEIGHT, false)
            .expect("Menlo ships with macOS");
        let bold_italic = ct
            .find_face("Menlo", BOLD_WEIGHT, true)
            .expect("Menlo ships with macOS");
        assert_ne!(regular, bold_italic);
        let face = ct.face(bold_italic).expect("recorded");
        assert_eq!(
            face.style,
            FaceStyle {
                italic: true,
                weight: BOLD_WEIGHT
            }
        );
    }

    #[test]
    fn a_hidden_system_family_is_not_accepted() {
        let ct = CoreText::system();
        assert!(ct.find_face(".SF Arabic", REGULAR_WEIGHT, false).is_none());
    }

    #[test]
    fn the_default_monospace_is_a_fixed_pitch_face() {
        let ct = CoreText::system();
        let (id, family) = ct
            .default_monospace()
            .expect("macOS has a fixed-pitch font");
        let face = ct.face(id).expect("recorded");
        let data = map_face_file(&face.path).expect("the face maps");
        assert!(ascii_is_fixed_pitch(&data, face.index), "{family}");
    }

    #[test]
    fn every_listed_family_is_accepted() {
        let ct = CoreText::system();
        let families = ct.installed_families();
        assert!(families.iter().any(|family| family.name == "Menlo"));
        for family in &families {
            assert!(
                ct.find_face(&family.name, REGULAR_WEIGHT, false).is_some(),
                "{} is listed but not accepted",
                family.name
            );
            assert!(!family.name.starts_with('.'), "{}", family.name);
        }
    }

    fn menlo_stack() -> FontStack {
        let ct = CoreText::system();
        FontStack::auto_discover_in(&ct, Some("Menlo"), &[], &[], &StyleFaces::default())
            .expect("Menlo ships with macOS")
    }

    #[test]
    fn a_scalar_menlo_lacks_draws_from_a_system_face_that_covers_it() {
        let stack = menlo_stack();
        for c in ['\u{0915}', '\u{0E01}', '\u{6F22}', '\u{23FA}'] {
            assert!(!stack.primary().has_glyph(c));
            let index = stack.resolve_styled_index(c, FontStyle::REGULAR);
            assert_ne!(index, 0, "{c}");
            assert!(stack.font_at(index, FontStyle::REGULAR).has_glyph(c), "{c}");
        }
    }

    #[test]
    fn an_emoji_presentation_scalar_draws_in_color_and_its_text_form_does_not() {
        let stack = menlo_stack();
        let emoji = stack.resolve_styled_index('\u{2B50}', FontStyle::REGULAR);
        assert!(stack.font_at(emoji, FontStyle::REGULAR).has_color_glyphs());
        let text = stack.first_cover_styled_index('\u{23FA}', FontStyle::REGULAR);
        assert!(!stack.font_at(text, FontStyle::REGULAR).has_color_glyphs());
    }

    #[test]
    fn the_language_picks_the_face_that_draws_han() {
        let answer = |language: &str| {
            let fallback = Fallback {
                primary: "Menlo".to_owned(),
                language: Some(language.to_owned()),
                files: Mutex::default(),
            };
            let face = fallback
                .face_for("\u{9AA8}", FontStyle::REGULAR)
                .expect("the lookup runs")
                .expect("macOS draws Han");
            let FaceSource::File(path) = face.source else {
                panic!("a CoreText face is a file");
            };
            (path, face.index)
        };
        assert_ne!(answer("ja"), answer("zh-Hans"));
    }

    #[test]
    fn a_bold_cell_asks_for_a_bold_fallback() {
        let stack = menlo_stack();
        let bold = FontStyle {
            bold: true,
            italic: false,
        };
        let weight = |style| {
            let index = stack.resolve_styled_index('\u{6F22}', style);
            let font = stack.font_at(index, style);
            font.font_ref().attributes().weight().0
        };
        assert!(weight(bold) > weight(FontStyle::REGULAR));
    }
}

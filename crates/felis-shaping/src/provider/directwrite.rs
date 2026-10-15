//! Face discovery through DirectWrite: its system font collection, its
//! family matching and its per-character fallback, so the faces drawn are
//! the ones the platform picks.

use std::{
    cell::RefCell, collections::HashMap, ffi::OsString, os::windows::ffi::OsStringExt,
    path::PathBuf, ptr,
};

use super::{
    AxisPosition, BOLD_WEIGHT, FaceProvider, FaceSource, MONOSPACE_FALLBACK_FAMILIES,
    REGULAR_WEIGHT, ResolvedFace, SystemFallback, ascii_is_fixed_pitch, closest_style,
    fallback_language, instance_at,
};
use crate::{FaceStyle, FontStyle, InstalledFamily, ShapingError, map_face_file};
use swash::FontDataRef;
use tracing::debug;
use windows::{
    System::UserProfile::GlobalizationPreferences,
    Win32::{
        Globalization::{GetUserDefaultLocaleName, GetUserPreferredUILanguages, MUI_LANGUAGE_NAME},
        Graphics::DirectWrite::{
            DWRITE_E_REMOTEFONT, DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_AXIS_VALUE,
            DWRITE_FONT_SIMULATIONS_NONE, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_ITALIC,
            DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT, DWriteCreateFactory, IDWriteFactory2,
            IDWriteFont, IDWriteFontCollection, IDWriteFontFace, IDWriteFontFace5,
            IDWriteFontFamily, IDWriteFontFile, IDWriteLocalFontFileLoader,
            IDWriteLocalizedStrings, IDWriteTextAnalysisSource,
        },
        System::SystemServices::LOCALE_NAME_MAX_LENGTH,
    },
    core::{HSTRING, Interface, w},
};

/// Windows Terminal's default face, ahead of the families every platform
/// tries.
const DEFAULT_MONOSPACE: &str = "Cascadia Mono";

#[derive(Clone)]
struct Face {
    path: PathBuf,
    family: String,
    index: u32,
    instance: Option<usize>,
    style: FaceStyle,
}

/// A face by file, collection index and named instance.
type FaceKey = (PathBuf, u32, Option<usize>);

pub(crate) struct DirectWrite {
    collection: IDWriteFontCollection,
    faces: RefCell<Vec<Face>>,
    ids: RefCell<HashMap<FaceKey, usize>>,
}

fn platform(error: &windows::core::Error) -> ShapingError {
    ShapingError::DirectWrite(error.message())
}

/// The shared factory, which DirectWrite hands every caller in the
/// process, and its system collection.
fn system_collection() -> Result<(IDWriteFactory2, IDWriteFontCollection), ShapingError> {
    // SAFETY: the call has no preconditions.
    let factory: IDWriteFactory2 =
        unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }.map_err(|e| platform(&e))?;
    let mut collection = None;
    // SAFETY: `collection` outlives the call that writes it.
    unsafe { factory.GetSystemFontCollection(&raw mut collection, false) }
        .map_err(|e| platform(&e))?;
    let collection =
        collection.ok_or_else(|| ShapingError::DirectWrite("no system collection".to_owned()))?;
    Ok((factory, collection))
}

impl DirectWrite {
    pub(crate) fn system() -> Result<Self, ShapingError> {
        let (_, collection) = system_collection()?;
        Ok(Self {
            collection,
            faces: RefCell::default(),
            ids: RefCell::default(),
        })
    }

    /// The faces DirectWrite holds for `family`, a localized name
    /// included, less those it would synthesize: felis draws a face's own
    /// outlines and does not embolden or slant them.
    fn family_faces(&self, family: &str) -> Vec<usize> {
        let (mut index, mut exists) = (0, false.into());
        // SAFETY: both out pointers outlive the call.
        let found = unsafe {
            self.collection
                .FindFamilyName(&HSTRING::from(family), &raw mut index, &raw mut exists)
        };
        if found.is_err() || !exists.as_bool() {
            return Vec::new();
        }
        // SAFETY: `index` is the one `FindFamilyName` just returned.
        let Ok(fonts) = (unsafe { self.collection.GetFontFamily(index) }) else {
            return Vec::new();
        };
        let canonical = family_names(&fonts).into_iter().next().unwrap_or_default();
        // SAFETY: the family is live.
        let count = unsafe { fonts.GetFontCount() };
        (0..count)
            // SAFETY: `at` is below the family's font count.
            .filter_map(|at| unsafe { fonts.GetFont(at) }.ok())
            // SAFETY: the font is live.
            .filter(|font| unsafe { font.GetSimulations() } == DWRITE_FONT_SIMULATIONS_NONE)
            .filter_map(|font| self.record(&font, &canonical))
            .collect()
    }

    fn record(&self, font: &IDWriteFont, family: &str) -> Option<usize> {
        // SAFETY: the font is live.
        let face = unsafe { font.CreateFontFace() }.ok()?;
        let (path, index, instance) = face_source(&face)?;
        // SAFETY: the font is live.
        let (weight, style) = unsafe { (font.GetWeight(), font.GetStyle()) };
        let style = FaceStyle {
            italic: style != DWRITE_FONT_STYLE_NORMAL,
            weight: u16::try_from(weight.0).unwrap_or(REGULAR_WEIGHT),
        };
        let key = (path.clone(), index, instance);
        let mut faces = self.faces.borrow_mut();
        Some(*self.ids.borrow_mut().entry(key).or_insert_with(|| {
            faces.push(Face {
                path,
                family: family.to_owned(),
                index,
                instance,
                style,
            });
            faces.len() - 1
        }))
    }

    fn face(&self, id: usize) -> Option<Face> {
        self.faces.borrow().get(id).cloned()
    }

    /// Every name, in every language, of every family in the collection:
    /// [`FaceProvider::find_face`] takes each of them.
    pub(crate) fn installed_families(&self) -> Vec<InstalledFamily> {
        // SAFETY: the collection is live.
        let count = unsafe { self.collection.GetFontFamilyCount() };
        let mut pitches = HashMap::new();
        let mut families = Vec::new();
        for at in 0..count {
            // SAFETY: `at` is below the collection's family count.
            let Ok(fonts) = (unsafe { self.collection.GetFontFamily(at) }) else {
                continue;
            };
            for name in family_names(&fonts) {
                let ids = self.family_faces(&name);
                if ids.is_empty() {
                    continue;
                }
                let mut family = InstalledFamily::new(name);
                for face in ids.iter().filter_map(|&id| self.face(id)) {
                    let monospace = *pitches
                        .entry((face.path.clone(), face.index))
                        .or_insert_with(|| {
                            map_face_file(&face.path)
                                .is_ok_and(|data| ascii_is_fixed_pitch(&data, face.index))
                        });
                    family.add_face(monospace, face.style);
                }
                families.push(family);
            }
        }
        families
            .sort_by_cached_key(|family| (family.name.to_ascii_lowercase(), family.name.clone()));
        families.dedup_by(|a, b| a.name.eq_ignore_ascii_case(&b.name));
        families
    }
}

impl FaceProvider for DirectWrite {
    type Id = usize;

    fn find_face(&self, family: &str, weight: u16, italic: bool) -> Option<usize> {
        let ids = self.family_faces(family);
        let styles: Vec<FaceStyle> = ids
            .iter()
            .filter_map(|&id| self.face(id).map(|face| face.style))
            .collect();
        closest_style(&styles, weight, italic).map(|at| ids[at])
    }

    /// DirectWrite names no default monospace face; Windows Terminal's
    /// is tried first.
    fn default_monospace(&self) -> Result<(usize, String), ShapingError> {
        std::iter::once(DEFAULT_MONOSPACE)
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
        let primary = self.face(primary)?;
        Some(Box::new(Fallback {
            locale: fallback_locale(),
            primary,
        }))
    }
}

/// `IDWriteFontFallback::MapCharacters` with the primary's family, the
/// cell's weight and slant, and `fallback_locale`. Its DirectWrite
/// objects are asked for again on every miss, which the shared factory
/// answers from its cache: they are not `Send`.
struct Fallback {
    primary: Face,
    /// NUL-terminated.
    locale: Vec<u16>,
}

impl SystemFallback for Fallback {
    fn name(&self) -> &'static str {
        "DirectWrite"
    }

    fn face_for(
        &self,
        query: &str,
        style: FontStyle,
    ) -> Result<Option<ResolvedFace>, ShapingError> {
        let (factory, collection) = system_collection()?;
        // SAFETY: the factory is live.
        let fallback = unsafe { factory.GetSystemFontFallback() }.map_err(|e| platform(&e))?;
        let text: Vec<u16> = query.encode_utf16().collect();
        let length = u32::try_from(text.len()).unwrap_or(u32::MAX);
        let source: IDWriteTextAnalysisSource = source::AnalysisSource {
            text,
            locale: self.locale.clone(),
        }
        .into();
        let weight = if style.bold {
            BOLD_WEIGHT
        } else {
            REGULAR_WEIGHT
        };
        let slant = if style.italic {
            DWRITE_FONT_STYLE_ITALIC
        } else {
            DWRITE_FONT_STYLE_NORMAL
        };
        let (mut mapped, mut font, mut scale) = (0, None, 1.0);
        // SAFETY: the out pointers outlive the call, and `source` owns the
        // text and locale buffers it hands DirectWrite for the call.
        unsafe {
            fallback.MapCharacters(
                &source,
                0,
                length,
                &collection,
                &HSTRING::from(self.primary.family.as_str()),
                DWRITE_FONT_WEIGHT(i32::from(weight)),
                slant,
                DWRITE_FONT_STRETCH_NORMAL,
                &raw mut mapped,
                &raw mut font,
                &raw mut scale,
            )
        }
        .map_err(|e| platform(&e))?;
        let Some(font) = font else {
            return Ok(None);
        };
        if (scale - 1.0).abs() > f32::EPSILON {
            debug!(
                query,
                scale, "DirectWrite scaled its fallback; drawing it unscaled"
            );
        }
        // SAFETY: the font is live.
        if unsafe { font.GetSimulations() } != DWRITE_FONT_SIMULATIONS_NONE {
            debug!(
                query,
                "DirectWrite synthesized its fallback; drawing its own outlines"
            );
        }
        // SAFETY: the font is live.
        let face = match unsafe { font.CreateFontFace() } {
            Ok(face) => Some(face),
            Err(error) if error.code() == DWRITE_E_REMOTEFONT => None,
            Err(error) => return Err(platform(&error)),
        };
        let Some((path, index, instance)) = face.as_ref().and_then(face_source) else {
            debug!(
                query,
                "the system fallback face is not a local file; drawing .notdef"
            );
            return Ok(None);
        };
        if path == self.primary.path && index == self.primary.index {
            return Ok(None);
        }
        // SAFETY: the font is live.
        let slanted = unsafe { font.GetStyle() } != DWRITE_FONT_STYLE_NORMAL;
        Ok(Some(ResolvedFace {
            source: FaceSource::File(path),
            index,
            instance,
            slanted,
        }))
    }
}

#[expect(
    clippy::inline_always,
    clippy::ref_as_ptr,
    trivial_casts,
    reason = "raised inside the code `#[implement]` expands to"
)]
mod source {
    use std::ptr;

    use windows::Win32::Graphics::DirectWrite::{
        DWRITE_READING_DIRECTION, DWRITE_READING_DIRECTION_LEFT_TO_RIGHT,
        IDWriteNumberSubstitution, IDWriteTextAnalysisSource, IDWriteTextAnalysisSource_Impl,
    };
    use windows_core::{OutRef, implement};

    /// The text and locale `MapCharacters` reads, owned for the call.
    #[implement(IDWriteTextAnalysisSource)]
    pub(super) struct AnalysisSource {
        pub(super) text: Vec<u16>,
        pub(super) locale: Vec<u16>,
    }

    impl IDWriteTextAnalysisSource_Impl for AnalysisSource_Impl {
        fn GetTextAtPosition(
            &self,
            position: u32,
            text: *mut *mut u16,
            length: *mut u32,
        ) -> windows::core::Result<()> {
            let rest = usize::try_from(position)
                .ok()
                .and_then(|at| self.text.get(at..))
                .unwrap_or_default();
            // SAFETY: DirectWrite passes writable out pointers; the buffer
            // lives as long as the source, which outlives the call.
            unsafe {
                *text = if rest.is_empty() {
                    ptr::null_mut()
                } else {
                    rest.as_ptr().cast_mut()
                };
                *length = u32::try_from(rest.len()).unwrap_or(0);
            }
            Ok(())
        }

        fn GetTextBeforePosition(
            &self,
            position: u32,
            text: *mut *mut u16,
            length: *mut u32,
        ) -> windows::core::Result<()> {
            let before = usize::try_from(position)
                .ok()
                .and_then(|at| self.text.get(..at))
                .unwrap_or_default();
            // SAFETY: as in `GetTextAtPosition`.
            unsafe {
                *text = if before.is_empty() {
                    ptr::null_mut()
                } else {
                    before.as_ptr().cast_mut()
                };
                *length = u32::try_from(before.len()).unwrap_or(0);
            }
            Ok(())
        }

        fn GetParagraphReadingDirection(&self) -> DWRITE_READING_DIRECTION {
            DWRITE_READING_DIRECTION_LEFT_TO_RIGHT
        }

        fn GetLocaleName(
            &self,
            position: u32,
            length: *mut u32,
            locale: *mut *mut u16,
        ) -> windows::core::Result<()> {
            let rest = u32::try_from(self.text.len())
                .unwrap_or(0)
                .saturating_sub(position);
            // SAFETY: as in `GetTextAtPosition`; the locale is NUL-terminated.
            unsafe {
                *length = rest;
                *locale = self.locale.as_ptr().cast_mut();
            }
            Ok(())
        }

        fn GetNumberSubstitution(
            &self,
            position: u32,
            length: *mut u32,
            substitution: OutRef<'_, IDWriteNumberSubstitution>,
        ) -> windows::core::Result<()> {
            let rest = u32::try_from(self.text.len())
                .unwrap_or(0)
                .saturating_sub(position);
            // SAFETY: as in `GetTextAtPosition`.
            unsafe { *length = rest };
            substitution.write(None)
        }
    }
}

/// The language `fallback_language` picks from the user's language list,
/// else `user_locale`, as the NUL-terminated name DirectWrite reads.
/// `GetUserPreferredUILanguages` is not the list: it names only the
/// display languages installed, so a Chinese reader on a Japanese or
/// English UI would draw Han in the wrong form.
fn fallback_locale() -> Vec<u16> {
    let languages: Vec<String> = GlobalizationPreferences::Languages()
        .and_then(|languages| {
            (0..languages.Size()?)
                .map(|at| languages.GetAt(at).map(|language| language.to_string()))
                .collect()
        })
        .unwrap_or_default();
    fallback_language(&languages).map_or_else(user_locale, |language| {
        language.encode_utf16().chain([0]).collect()
    })
}

/// The user's first preferred UI language, else their locale, as the
/// NUL-terminated name DirectWrite reads.
fn user_locale() -> Vec<u16> {
    let (mut count, mut size) = (0, 0);
    // SAFETY: a `None` buffer asks for the size; both out pointers outlive
    // the call.
    let sized = unsafe {
        GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &raw mut count, None, &raw mut size)
    };
    if sized.is_ok() && size > 0 {
        let mut buffer = vec![0_u16; size as usize];
        // SAFETY: `buffer` holds the `size` units the first call asked for.
        let filled = unsafe {
            GetUserPreferredUILanguages(
                MUI_LANGUAGE_NAME,
                &raw mut count,
                Some(windows::core::PWSTR(buffer.as_mut_ptr())),
                &raw mut size,
            )
        };
        if filled.is_ok()
            && let Some(end) = buffer.iter().position(|&unit| unit == 0)
            && end > 0
        {
            buffer.truncate(end + 1);
            return buffer;
        }
    }
    let mut buffer = vec![0_u16; LOCALE_NAME_MAX_LENGTH as usize];
    // SAFETY: the buffer is as long as the call is told.
    let written = unsafe { GetUserDefaultLocaleName(&mut buffer) };
    buffer.truncate(usize::try_from(written).unwrap_or(0).max(1));
    if buffer.last() != Some(&0) {
        buffer.push(0);
    }
    buffer
}

/// Every string of `strings`, in every language.
fn localized(strings: &IDWriteLocalizedStrings) -> Vec<String> {
    // SAFETY: the strings are live.
    let count = unsafe { strings.GetCount() };
    (0..count)
        .filter_map(|at| {
            // SAFETY: `at` is below the count.
            let length = unsafe { strings.GetStringLength(at) }.ok()?;
            let mut buffer = vec![0_u16; length as usize + 1];
            // SAFETY: the buffer holds the string and its NUL.
            unsafe { strings.GetString(at, &mut buffer) }.ok()?;
            buffer.truncate(length as usize);
            Some(String::from_utf16_lossy(&buffer))
        })
        .collect()
}

/// The family's names in every language, English first: the name a
/// styled lookup of one of its faces asks for.
fn family_names(family: &IDWriteFontFamily) -> Vec<String> {
    // SAFETY: the family is live.
    let Ok(strings) = (unsafe { family.GetFamilyNames() }) else {
        return Vec::new();
    };
    let mut names = localized(&strings);
    let (mut index, mut exists) = (0, false.into());
    // SAFETY: both out pointers outlive the call.
    let found = unsafe { strings.FindLocaleName(w!("en-us"), &raw mut index, &raw mut exists) };
    if found.is_ok()
        && exists.as_bool()
        && let Ok(index) = usize::try_from(index)
        && index < names.len()
    {
        let english = names.remove(index);
        names.insert(0, english);
    }
    names
}

/// The local file a face is drawn from, its index in the file, and the
/// named instance whose axis position DirectWrite draws it at; `None` for
/// a face no single local file holds, or one at an unnamed position.
fn face_source(face: &IDWriteFontFace) -> Option<(PathBuf, u32, Option<usize>)> {
    let mut count = 0;
    // SAFETY: a `None` array asks for the count.
    unsafe { face.GetFiles(&raw mut count, None) }.ok()?;
    if count != 1 {
        return None;
    }
    let mut files: Vec<Option<IDWriteFontFile>> = vec![None];
    // SAFETY: `files` holds the `count` slots the first call reported.
    unsafe { face.GetFiles(&raw mut count, Some(files.as_mut_ptr())) }.ok()?;
    let file = files.into_iter().next().flatten()?;
    let path = local_path(&file)?;
    // SAFETY: the face is live.
    let index = unsafe { face.GetIndex() };
    let instance = match face.cast::<IDWriteFontFace5>() {
        Ok(face) => match named_instance(&face, &path, index)? {
            AxisPosition::Default => None,
            AxisPosition::Named(at) => Some(at),
        },
        Err(_) => None,
    };
    Some((path, index, instance))
}

fn local_path(file: &IDWriteFontFile) -> Option<PathBuf> {
    // SAFETY: the file is live.
    let loader = unsafe { file.GetLoader() }.ok()?;
    let local: IDWriteLocalFontFileLoader = loader.cast().ok()?;
    let (mut key, mut key_size) = (ptr::null_mut(), 0);
    // SAFETY: the key DirectWrite hands back lives as long as `file`.
    unsafe { file.GetReferenceKey(&raw mut key, &raw mut key_size) }.ok()?;
    // SAFETY: `key` and `key_size` came from the file the loader serves.
    let length = unsafe { local.GetFilePathLengthFromKey(key, key_size) }.ok()?;
    let mut buffer = vec![0_u16; length as usize + 1];
    // SAFETY: the buffer holds the path and its NUL.
    unsafe { local.GetFilePathFromKey(key, key_size, &mut buffer) }.ok()?;
    buffer.truncate(length as usize);
    Some(PathBuf::from(OsString::from_wide(&buffer)))
}

/// The `fvar` named instance at the axis position DirectWrite draws the
/// face at, as [`instance_at`] reads it.
fn named_instance(
    face: &IDWriteFontFace5,
    path: &std::path::Path,
    index: u32,
) -> Option<AxisPosition> {
    // SAFETY: the face is live.
    if !unsafe { face.HasVariations() }.as_bool() {
        return Some(AxisPosition::Default);
    }
    // SAFETY: the face is live.
    let count = unsafe { face.GetFontAxisValueCount() };
    let mut values = vec![DWRITE_FONT_AXIS_VALUE::default(); count as usize];
    // SAFETY: `values` holds `count` entries.
    unsafe { face.GetFontAxisValues(&mut values) }.ok()?;
    // DirectWrite packs a tag little-endian, OpenType big-endian.
    let position: Vec<(u32, f32)> = values
        .iter()
        .map(|value| (value.axisTag.0.swap_bytes(), value.value))
        .collect();
    let data = map_face_file(path).ok()?;
    let font = FontDataRef::new(&data)?.get(usize::try_from(index).ok()?)?;
    instance_at(font, &position)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_family_is_accepted() {
        let dw = DirectWrite::system().expect("DirectWrite loads");
        let families = dw.installed_families();
        assert_ne!(families, []);
        for family in &families {
            assert!(
                dw.find_face(&family.name, REGULAR_WEIGHT, false).is_some(),
                "{} is listed but not accepted",
                family.name
            );
        }
        assert!(
            dw.find_face("felis no such family", REGULAR_WEIGHT, false)
                .is_none()
        );
    }

    #[test]
    fn the_default_monospace_is_a_fixed_pitch_face_on_disk() {
        let dw = DirectWrite::system().expect("DirectWrite loads");
        let (id, family) = dw
            .default_monospace()
            .expect("Windows has a fixed-pitch font");
        let face = dw.face(id).expect("recorded");
        let data = map_face_file(&face.path).expect("the face maps");
        assert!(ascii_is_fixed_pitch(&data, face.index), "{family}");
    }

    /// Which face answers is the runner image's business; that an answer
    /// loads and covers what was asked is the provider's.
    #[test]
    fn a_fallback_answer_loads_and_covers_the_scalar() {
        let dw = DirectWrite::system().expect("DirectWrite loads");
        let (id, _) = dw
            .default_monospace()
            .expect("Windows has a fixed-pitch font");
        let fallback = dw.system_fallback(id).expect("DirectWrite has a fallback");
        for query in ["\u{23FA}", "\u{1F600}\u{FE0F}", "\u{6F22}", "\u{0915}"] {
            let Some(face) = fallback
                .face_for(query, FontStyle::REGULAR)
                .expect("the lookup runs")
            else {
                assert_ne!(query, "\u{6F22}", "Windows draws Han");
                continue;
            };
            let font = crate::Font::load_resolved(face).expect("the answer loads");
            let c = query.chars().next().expect("non-empty");
            assert!(font.has_glyph(c), "{query}");
        }
    }

    #[test]
    fn the_language_picks_the_face_that_draws_han() {
        let dw = DirectWrite::system().expect("DirectWrite loads");
        if ["Yu Gothic", "Microsoft YaHei"]
            .iter()
            .any(|family| dw.find_face(family, REGULAR_WEIGHT, false).is_none())
        {
            return;
        }
        let (id, _) = dw
            .default_monospace()
            .expect("Windows has a fixed-pitch font");
        let answer = |language: &str| {
            let fallback = Fallback {
                primary: dw.face(id).expect("recorded"),
                locale: language.encode_utf16().chain([0]).collect(),
            };
            let face = fallback
                .face_for("\u{9AA8}", FontStyle::REGULAR)
                .expect("the lookup runs")
                .expect("Windows draws Han");
            let FaceSource::File(path) = face.source else {
                panic!("a DirectWrite face is a file");
            };
            (path, face.index)
        };
        assert_ne!(answer("ja-JP"), answer("zh-CN"));
    }

    #[test]
    fn a_bold_cell_asks_for_a_bold_fallback() {
        let stack = crate::FontStack::auto_discover(None, &[], &[], &crate::StyleFaces::default())
            .expect("the default stack loads");
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

    #[test]
    fn the_fallback_and_user_locales_are_nul_terminated_names() {
        for locale in [fallback_locale(), user_locale()] {
            assert_eq!(locale.last(), Some(&0));
            assert!(locale.len() > 1);
        }
    }
}

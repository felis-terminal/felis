//! Face discovery through the host's libfontconfig, so the faces drawn are
//! the ones `fc-match` names among the formats the loader parses, and the
//! user's `fonts.conf` rules apply.

use std::{
    cell::{OnceCell, RefCell},
    collections::HashMap,
    ffi::{CStr, CString, c_int},
    mem::ManuallyDrop,
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    ptr,
    sync::OnceLock,
};

use fontconfig_sys::{
    Fc, FcBool, FcCharSet, FcConfig, FcFontSet, FcMatchPattern, FcPattern, FcResult, FcResultMatch,
    FcSetApplication, FcSetSystem,
    constants::{
        FC_CHARSET, FC_COLOR, FC_FAMILY, FC_FILE, FC_FONTFORMAT, FC_INDEX, FC_LANG, FC_SLANT,
        FC_SLANT_ITALIC, FC_SLANT_ROMAN, FC_WEIGHT,
    },
};

use super::{CascadeFace, FaceProvider, FaceSource, LoadedAs, REGULAR_WEIGHT, ResolvedFace};
use crate::ShapingError;

const FC_FALSE: FcBool = 0;
#[cfg(test)]
const FC_TRUE: FcBool = 1;

#[cfg(target_os = "macos")]
const SONAME: &str = "libfontconfig.1.dylib";
#[cfg(not(target_os = "macos"))]
const SONAME: &str = "libfontconfig.so.1";

/// fontconfig's packed `FC_INDEX` instance number for a variable face at
/// its default position (`fcfreetype.c`).
const VARIABLE_DEFAULT_INSTANCE: u32 = 0x8000;

/// A family no font declares; the config rules it picks up are the ones
/// every name gets.
const UNKNOWN_FAMILY: &str = "felis-unknown-family-sentinel";

/// The `fontformat` of the sfnt containers the loader parses; fontconfig
/// also lists PCF, BDF and Type 1 faces it cannot.
const LOADABLE_FORMATS: [&CStr; 2] = [c"TrueType", c"CFF"];

/// Loaded once per process; never unloaded, so the function pointers in
/// `Fc` stay valid for `'static`.
fn library() -> Result<&'static Fc, ShapingError> {
    static LIB: OnceLock<Result<Fc, String>> = OnceLock::new();
    LIB.get_or_init(|| {
        // SAFETY: loading libfontconfig runs only its ELF/Mach-O
        // initializers, which have no preconditions; every symbol is
        // resolved against the binding's declared signatures, and a
        // missing one fails the load instead of leaving a dangling field.
        unsafe { Fc::open(SONAME) }.map_err(|e| e.to_string())
    })
    .as_ref()
    .map_err(|detail| ShapingError::Fontconfig(format!("load {SONAME}: {detail}")))
}

pub(crate) struct Fontconfig {
    lib: &'static Fc,
    config: *mut FcConfig,
    /// Set by tests so a query does not depend on the host's locale.
    lang: Option<CString>,
    faces: RefCell<Vec<Face>>,
    ids: RefCell<HashMap<FaceKey, usize>>,
    /// The installed faces the loader can parse, matched and sorted in
    /// place of the config's own sets so a face it cannot draw is never
    /// chosen, as if it were not installed.
    loadable: OnceCell<FontSet<'static>>,
    /// Every loadable face under each family name it carries, so a
    /// fallback's styles are matched among its family alone instead of
    /// against the whole font set once per style.
    families: OnceCell<HashMap<Vec<u8>, FontSet<'static>>>,
    /// An unknown name's substituted query, less the name; keyed by
    /// (weight, italic) since a rule can test either.
    unknown: RefCell<HashMap<(u16, bool), Pattern<'static>>>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct FaceKey {
    path: PathBuf,
    index: u32,
}

struct Face {
    key: FaceKey,
    family: String,
    slanted: bool,
}

/// A pattern this module created or was handed ownership of.
struct Pattern<'a> {
    lib: &'a Fc,
    raw: *mut FcPattern,
}

impl Drop for Pattern<'_> {
    fn drop(&mut self) {
        // SAFETY: `raw` is owned and non-null by construction.
        unsafe { (self.lib.FcPatternDestroy)(self.raw) }
    }
}

struct FontSet<'a> {
    lib: &'a Fc,
    raw: *mut FcFontSet,
}

impl<'a> FontSet<'a> {
    fn new(lib: &'a Fc) -> Self {
        // SAFETY: no preconditions; the set is owned by the guard.
        let raw = unsafe { (lib.FcFontSetCreate)() };
        assert!(!raw.is_null(), "FcFontSetCreate failed");
        Self { lib, raw }
    }

    fn add(&self, pattern: *mut FcPattern) {
        // SAFETY: both are live; the reference taken here is the one the
        // set releases when destroyed.
        unsafe {
            (self.lib.FcPatternReference)(pattern);
            (self.lib.FcFontSetAdd)(self.raw, pattern);
        }
    }

    fn patterns(&self) -> impl Iterator<Item = *mut FcPattern> + '_ {
        // SAFETY: `raw` is a non-null set fontconfig returned; a non-empty
        // set's `fonts` holds `nfont` patterns that live as long as the
        // set, and an empty one may leave `fonts` null.
        let fonts = unsafe {
            let set = &*self.raw;
            match usize::try_from(set.nfont) {
                Ok(len) if len > 0 && !set.fonts.is_null() => {
                    std::slice::from_raw_parts(set.fonts, len)
                }
                _ => &[],
            }
        };
        fonts.iter().copied()
    }
}

impl Drop for FontSet<'_> {
    fn drop(&mut self) {
        // SAFETY: `raw` is owned and non-null by construction.
        unsafe { (self.lib.FcFontSetDestroy)(self.raw) }
    }
}

struct CharSet<'a> {
    lib: &'a Fc,
    raw: *mut FcCharSet,
}

impl Drop for CharSet<'_> {
    fn drop(&mut self) {
        // SAFETY: `raw` is owned and non-null by construction.
        unsafe { (self.lib.FcCharSetDestroy)(self.raw) }
    }
}

impl Fontconfig {
    /// The host's configuration and fonts, as `fc-match` sees them.
    #[cfg(target_os = "linux")]
    pub(crate) fn system() -> Result<Self, ShapingError> {
        let lib = library()?;
        // SAFETY: no preconditions; the returned config is owned here.
        let config = unsafe { (lib.FcInitLoadConfigAndFonts)() };
        if config.is_null() {
            return Err(ShapingError::Fontconfig(
                "FcInitLoadConfigAndFonts returned no configuration".into(),
            ));
        }
        Ok(Self::with_config(lib, config, None))
    }

    /// A configuration holding only `font_dir` and the rules in
    /// `rules_file`, queried as if the locale were `lang`.
    #[cfg(test)]
    pub(crate) fn isolated(
        font_dir: &std::path::Path,
        rules_file: Option<&std::path::Path>,
        lang: &str,
    ) -> Result<Self, ShapingError> {
        let lib = library()?;
        let fail = |what: &str| ShapingError::Fontconfig(format!("isolated config: {what}"));
        // SAFETY: no preconditions; the returned config is owned here.
        let config = unsafe { (lib.FcConfigCreate)() };
        if config.is_null() {
            return Err(fail("FcConfigCreate"));
        }
        let this = Self::with_config(lib, config, Some(c_string(lang)));
        let dir = c_path(font_dir);
        // SAFETY: `config` is live; `dir` outlives the call.
        if unsafe { (lib.FcConfigAppFontAddDir)(config, dir.as_ptr().cast()) } == FC_FALSE {
            return Err(fail("FcConfigAppFontAddDir"));
        }
        if let Some(rules) = rules_file {
            let rules = c_path(rules);
            // SAFETY: as above.
            if unsafe { (lib.FcConfigParseAndLoad)(config, rules.as_ptr().cast(), FC_TRUE) }
                == FC_FALSE
            {
                return Err(fail("FcConfigParseAndLoad"));
            }
        }
        Ok(this)
    }

    fn with_config(lib: &'static Fc, config: *mut FcConfig, lang: Option<CString>) -> Self {
        Self {
            lib,
            config,
            lang,
            faces: RefCell::new(Vec::new()),
            ids: RefCell::new(HashMap::new()),
            loadable: OnceCell::new(),
            families: OnceCell::new(),
            unknown: RefCell::new(HashMap::new()),
        }
    }

    /// A query for `family`, with the config's and fontconfig's default
    /// substitutions applied (the config's adds the locale's `lang`).
    fn query(&self, family: &str, weight: u16, italic: bool) -> Pattern<'_> {
        let lib = self.lib;
        // SAFETY: every pointer passed is live for the call; the pattern is
        // owned by the returned guard.
        unsafe {
            let raw = (lib.FcPatternCreate)();
            assert!(!raw.is_null(), "FcPatternCreate failed");
            let pattern = Pattern { lib, raw };
            let family = c_string(family);
            (lib.FcPatternAddString)(raw, FC_FAMILY.as_ptr(), family.as_ptr().cast());
            (lib.FcPatternAddInteger)(raw, FC_WEIGHT.as_ptr(), fc_weight(weight));
            let slant = if italic {
                FC_SLANT_ITALIC
            } else {
                FC_SLANT_ROMAN
            };
            (lib.FcPatternAddInteger)(raw, FC_SLANT.as_ptr(), slant);
            if let Some(lang) = &self.lang {
                (lib.FcPatternAddString)(raw, FC_LANG.as_ptr(), lang.as_ptr().cast());
            }
            (lib.FcConfigSubstitute)(self.config, raw, FcMatchPattern);
            (lib.FcDefaultSubstitute)(raw);
            pattern
        }
    }

    fn best_match(&self, query: &Pattern<'_>) -> Option<Pattern<'_>> {
        self.match_in(self.loadable(), query)
    }

    fn match_in(&self, set: &FontSet<'_>, query: &Pattern<'_>) -> Option<Pattern<'_>> {
        let mut sets = [set.raw];
        let mut result: FcResult = 0;
        // SAFETY: `config`, the set and `query` are live; the match is
        // owned by the returned guard.
        let raw = unsafe {
            (self.lib.FcFontSetMatch)(
                self.config,
                sets.as_mut_ptr(),
                1,
                query.raw,
                &raw mut result,
            )
        };
        (!raw.is_null()).then_some(Pattern { lib: self.lib, raw })
    }

    /// Whether an installed face carries `name` as its family; fontconfig
    /// compares ignoring case and blanks. A full or PostScript name is not
    /// enough: the query names a family, so it would match another face.
    fn is_installed(&self, name: &str) -> bool {
        let name = c_string(name);
        // SAFETY: every pointer passed is live for the call; the pattern,
        // object set and result are destroyed here.
        unsafe {
            let raw = (self.lib.FcPatternCreate)();
            assert!(!raw.is_null(), "FcPatternCreate failed");
            let pattern = Pattern { lib: self.lib, raw };
            (self.lib.FcPatternAddString)(raw, FC_FAMILY.as_ptr(), name.as_ptr().cast());
            let objects = (self.lib.FcObjectSetCreate)();
            (self.lib.FcObjectSetAdd)(objects, FC_FILE.as_ptr());
            let mut sets = [self.loadable().raw];
            let set =
                (self.lib.FcFontSetList)(self.config, sets.as_mut_ptr(), 1, pattern.raw, objects);
            (self.lib.FcObjectSetDestroy)(objects);
            if set.is_null() {
                return false;
            }
            let set = FontSet {
                lib: self.lib,
                raw: set,
            };
            set.patterns().next().is_some()
        }
    }

    /// A copy of `query` with `name` taken out of its family list.
    fn without_family(&self, query: &Pattern<'_>, name: &str) -> Pattern<'static> {
        let lib = self.lib;
        // SAFETY: `query` is live; the copy is owned by the returned guard.
        let copy = Pattern {
            lib,
            raw: unsafe { (lib.FcPatternDuplicate)(query.raw) },
        };
        assert!(!copy.raw.is_null(), "FcPatternDuplicate failed");
        while let Some(at) = (0..)
            .map_while(|n| Some((n, self.string_at(copy.raw, FC_FAMILY, n)?)))
            .find_map(|(n, f)| (f.to_bytes() == name.as_bytes()).then_some(n))
        {
            // SAFETY: `copy` is live and `at` indexes an existing value.
            unsafe { (lib.FcPatternRemove)(copy.raw, FC_FAMILY.as_ptr(), at) };
        }
        copy
    }

    /// Whether config substitution treated `name` exactly as it treats a
    /// name no rule mentions.
    fn substitutes_as_unknown(
        &self,
        query: &Pattern<'_>,
        name: &str,
        weight: u16,
        italic: bool,
    ) -> bool {
        let stripped = self.without_family(query, name);
        let mut unknown = self.unknown.borrow_mut();
        let sentinel = unknown.entry((weight, italic)).or_insert_with(|| {
            self.without_family(&self.query(UNKNOWN_FAMILY, weight, italic), UNKNOWN_FAMILY)
        });
        // SAFETY: both patterns are live.
        unsafe { (self.lib.FcPatternEqual)(stripped.raw, sentinel.raw) != FC_FALSE }
    }

    fn loadable(&self) -> &FontSet<'static> {
        self.loadable.get_or_init(|| {
            let lib = self.lib;
            let loadable = FontSet::new(lib);
            for which in [FcSetSystem, FcSetApplication] {
                // SAFETY: `config` is live; the set stays owned by it.
                let raw = unsafe { (lib.FcConfigGetFonts)(self.config, which) };
                if raw.is_null() {
                    continue;
                }
                let installed = ManuallyDrop::new(FontSet { lib, raw });
                for pattern in installed.patterns() {
                    if self
                        .string(pattern, FC_FONTFORMAT)
                        .is_some_and(|format| LOADABLE_FORMATS.contains(&format))
                    {
                        loadable.add(pattern);
                    }
                }
            }
            loadable
        })
    }

    fn families(&self) -> &HashMap<Vec<u8>, FontSet<'static>> {
        self.families.get_or_init(|| {
            let mut families: HashMap<Vec<u8>, FontSet<'static>> = HashMap::new();
            for pattern in self.loadable().patterns() {
                for family in (0..).map_while(|n| self.string_at(pattern, FC_FAMILY, n)) {
                    families
                        .entry(family.to_bytes().to_vec())
                        .or_insert_with(|| FontSet::new(self.lib))
                        .add(pattern);
                }
            }
            families
        })
    }

    fn face_of(&self, pattern: *mut FcPattern) -> Option<Face> {
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(
            self.string(pattern, FC_FILE)?.to_bytes(),
        ));
        let index = self.integer(pattern, FC_INDEX).unwrap_or(0).cast_unsigned();
        let family = self
            .string(pattern, FC_FAMILY)
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        let slanted = self
            .integer(pattern, FC_SLANT)
            .is_some_and(|s| s != FC_SLANT_ROMAN);
        Some(Face {
            key: FaceKey { path, index },
            family,
            slanted,
        })
    }

    fn record(&self, face: Face) -> usize {
        let mut faces = self.faces.borrow_mut();
        *self
            .ids
            .borrow_mut()
            .entry(face.key.clone())
            .or_insert_with(|| {
                faces.push(face);
                faces.len() - 1
            })
    }

    fn string(&self, pattern: *mut FcPattern, object: &CStr) -> Option<&CStr> {
        self.string_at(pattern, object, 0)
    }

    fn string_at(&self, pattern: *mut FcPattern, object: &CStr, n: c_int) -> Option<&CStr> {
        let mut value = ptr::null_mut();
        // SAFETY: `pattern` is live; the string it hands back is owned by
        // the pattern, which the caller keeps alive past the borrow.
        let found =
            unsafe { (self.lib.FcPatternGetString)(pattern, object.as_ptr(), n, &raw mut value) };
        (found == FcResultMatch && !value.is_null())
            // SAFETY: fontconfig strings are NUL-terminated.
            .then(|| unsafe { CStr::from_ptr(value.cast()) })
    }

    fn integer(&self, pattern: *mut FcPattern, object: &CStr) -> Option<c_int> {
        let mut value = 0;
        // SAFETY: `pattern` is live.
        let found =
            unsafe { (self.lib.FcPatternGetInteger)(pattern, object.as_ptr(), 0, &raw mut value) };
        (found == FcResultMatch).then_some(value)
    }

    fn boolean(&self, pattern: *mut FcPattern, object: &CStr) -> Option<bool> {
        let mut value: FcBool = FC_FALSE;
        // SAFETY: `pattern` is live.
        let found =
            unsafe { (self.lib.FcPatternGetBool)(pattern, object.as_ptr(), 0, &raw mut value) };
        (found == FcResultMatch).then_some(value != FC_FALSE)
    }
}

impl Drop for Fontconfig {
    fn drop(&mut self) {
        // SAFETY: the config is owned here and nothing borrowed from it
        // outlives `self`.
        unsafe { (self.lib.FcConfigDestroy)(self.config) }
    }
}

impl FaceProvider for Fontconfig {
    type Id = usize;

    /// A name no installed face carries is still accepted when a config
    /// rule edits its query beyond what an unknown name gets, so an
    /// `<alias>` in `fonts.conf` is honored rather than reported missing.
    /// Comparing the matched face instead misfires when the alias target
    /// is also where an unknown name lands.
    fn find_face(&self, family: &str, weight: u16, italic: bool) -> Option<usize> {
        let query = self.query(family, weight, italic);
        if !self.is_installed(family) && self.substitutes_as_unknown(&query, family, weight, italic)
        {
            return None;
        }
        let found = self.face_of(self.best_match(&query)?.raw)?;
        Some(self.record(found))
    }

    /// Matched among the faces carrying `family` alone when `family` is
    /// the one `id` itself carries, the case for every face of the
    /// cascade; any other name is looked up as `find_face` would.
    fn find_sibling(&self, id: usize, family: &str, weight: u16, italic: bool) -> Option<usize> {
        if self.family_of(id).as_deref() != Some(family) {
            return self.find_face(family, weight, italic);
        }
        let set = self.families().get(family.as_bytes())?;
        let query = self.query(family, weight, italic);
        let matched = self.match_in(set, &query)?;
        let found = self.face_of(matched.raw)?;
        Some(self.record(found))
    }

    fn default_monospace(&self) -> Result<(usize, String), ShapingError> {
        let query = self.query("monospace", REGULAR_WEIGHT, false);
        let face = self
            .best_match(&query)
            .and_then(|m| self.face_of(m.raw))
            .ok_or(ShapingError::NoFont)?;
        let family = face.family.clone();
        Ok((self.record(face), family))
    }

    fn resolve_face(&self, id: usize) -> Option<ResolvedFace> {
        let faces = self.faces.borrow();
        let face = faces.get(id)?;
        let (index, instance) = split_index(face.key.index);
        Some(ResolvedFace {
            source: FaceSource::File(face.key.path.clone()),
            index,
            instance,
            slanted: face.slanted,
        })
    }

    fn family_of(&self, id: usize) -> Option<String> {
        self.faces.borrow().get(id).map(|f| f.family.clone())
    }

    /// fontconfig's ranked fallback, cut to the faces that add coverage
    /// within their presentation class. Untrimmed: `FcFontSetSort`'s trim
    /// drops a color face whose code points an earlier mono face covers.
    /// The primary's face stays in: a bold or italic override of another
    /// family may lack glyphs it has.
    fn fallback_cascade(
        &self,
        requested: &str,
        loaded: &HashMap<usize, LoadedAs>,
    ) -> Option<Vec<CascadeFace<usize>>> {
        let lib = self.lib;
        let query = self.query(requested, REGULAR_WEIGHT, false);
        let mut sets = [self.loadable().raw];
        let mut result: FcResult = 0;
        // SAFETY: `config`, the set and `query` are live; the sorted set
        // is owned by the guard and every charset is created and
        // destroyed here.
        let (sorted, mono, color) = unsafe {
            let raw = (lib.FcFontSetSort)(
                self.config,
                sets.as_mut_ptr(),
                1,
                query.raw,
                FC_FALSE,
                ptr::null_mut(),
                &raw mut result,
            );
            if raw.is_null() {
                return Some(Vec::new());
            }
            let mono = (lib.FcCharSetCreate)();
            let color = (lib.FcCharSetCreate)();
            assert!(
                !mono.is_null() && !color.is_null(),
                "FcCharSetCreate failed"
            );
            (
                FontSet { lib, raw },
                CharSet { lib, raw: mono },
                CharSet { lib, raw: color },
            )
        };
        let mut cascade = Vec::new();
        for pattern in sorted.patterns() {
            let Some(face) = self.face_of(pattern) else {
                continue;
            };
            let mut charset = ptr::null_mut();
            // SAFETY: `pattern` lives as long as `sorted`; the charset is
            // borrowed from it and only read by the merge below.
            let has_charset = unsafe {
                (lib.FcPatternGetCharSet)(pattern, FC_CHARSET.as_ptr(), 0, &raw mut charset)
            } == FcResultMatch;
            if !has_charset || charset.is_null() {
                continue;
            }
            let id = self.record(face);
            let is_color = match loaded.get(&id) {
                Some(LoadedAs::Unusable) => continue,
                Some(LoadedAs::Color(color)) => *color,
                None => self.boolean(pattern, FC_COLOR).unwrap_or(false),
            };
            let class = if is_color { &color } else { &mono };
            let mut grew: FcBool = FC_FALSE;
            // SAFETY: both charsets are live.
            unsafe { (lib.FcCharSetMerge)(class.raw, charset, &raw mut grew) };
            if grew != FC_FALSE && !cascade.iter().any(|c: &CascadeFace<usize>| c.id == id) {
                cascade.push(CascadeFace {
                    id,
                    color: is_color,
                });
            }
        }
        Some(cascade)
    }
}

/// `FC_INDEX` packs a named instance above the face index:
/// `(instance << 16) + face`, instance `0` meaning none, `0x8000` the
/// variable default, and `n` named instance `n - 1` in `fvar` order.
fn split_index(packed: u32) -> (u32, Option<usize>) {
    let face = packed & 0xFFFF;
    let instance = packed >> 16;
    let named = (instance != 0 && instance != VARIABLE_DEFAULT_INSTANCE)
        .then(|| usize::try_from(instance - 1).ok())
        .flatten();
    (face, named)
}

/// OpenType `usWeightClass` to fontconfig's weight scale, at the nearest
/// named weight: only 400 and 700 are asked for, so the table needs no
/// interpolation.
fn fc_weight(weight: u16) -> c_int {
    const SCALE: [(u16, c_int); 9] = [
        (100, 0),
        (200, 40),
        (300, 50),
        (400, 80),
        (500, 100),
        (600, 180),
        (700, 200),
        (800, 205),
        (900, 210),
    ];
    SCALE
        .iter()
        .min_by_key(|(ot, _)| ot.abs_diff(weight))
        .map_or(80, |&(_, fc)| fc)
}

fn c_string(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}

#[cfg(test)]
fn c_path(path: &std::path::Path) -> CString {
    CString::new(path.as_os_str().as_bytes().to_vec()).unwrap_or_default()
}

/// The pinned font set under `rules` (the body of a `<fontconfig>`
/// document), queried as `lang`. `None` skips where libfontconfig is
/// absent; Linux always has it, so it fails there.
#[cfg(test)]
pub(crate) fn pinned_config(rules: &str, lang: &str) -> Option<Fontconfig> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = crate::test_font_dir();
    let file = std::env::temp_dir().join(format!(
        "felis-fontconfig-{}-{}.conf",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(
        &file,
        format!("<?xml version=\"1.0\"?>\n<fontconfig>\n{rules}\n</fontconfig>\n"),
    )
    .expect("the rules file writes");
    let config = Fontconfig::isolated(&dir, Some(&file), lang);
    std::fs::remove_file(&file).ok();
    match config {
        Ok(config) => Some(config),
        Err(e) if cfg!(target_os = "linux") => panic!("{e}"),
        Err(e) => {
            eprintln!("{e}; skipping");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::BOLD_WEIGHT;
    use crate::{FaceSpec, FontStack, FontStyle, Shaper, StyleFaces, TEST_FONT_FAMILY};

    fn pinned(rules: &str) -> Option<Fontconfig> {
        pinned_config(rules, "ja")
    }

    fn family(fc: &Fontconfig, id: usize) -> String {
        fc.family_of(id).expect("a recorded face")
    }

    /// Ranks the two faces that both cover the dingbats and emoji below
    /// right after the primary, in the given order.
    fn ranked_stack(first: &str, second: &str) -> Option<FontStack> {
        let fc = pinned(&format!(
            r"<match target='pattern'><edit name='family' mode='append' binding='strong'>
              <string>{first}</string><string>{second}</string></edit></match>"
        ))?;
        Some(
            FontStack::auto_discover_in(
                &fc,
                Some(TEST_FONT_FAMILY),
                &[],
                &[],
                &StyleFaces::default(),
            )
            .expect("the pinned primary loads"),
        )
    }

    fn drawn_in_color(stack: &FontStack, text: &str) -> bool {
        let id = Shaper::new()
            .shape_cluster(stack, 16.0, FontStyle::REGULAR, text)
            .font_id;
        stack.font_at(id, FontStyle::REGULAR).has_color_glyphs()
    }

    /// Which face the cascade ranks after the primary's own entry (slot
    /// 1), with the `monospace` alias preferring `first` then `second`.
    fn first_fallback_is_color(first: &str, second: &str) -> Option<bool> {
        let fc = pinned(&format!(
            r"<alias><family>monospace</family><prefer><family>Monaspace Xenon</family>
              <family>{first}</family><family>{second}</family></prefer></alias>"
        ))?;
        let stack = FontStack::auto_discover_in(&fc, None, &[], &[], &StyleFaces::default())
            .expect("the monospace alias resolves");
        assert!(
            stack.len() > 3,
            "the alias's fallbacks must be in the cascade"
        );
        assert!(stack.font_at(1, FontStyle::REGULAR).has_glyph('A'));
        Some(stack.font_at(2, FontStyle::REGULAR).has_color_glyphs())
    }

    #[test]
    fn a_packed_index_splits_into_face_and_zero_based_instance() {
        assert_eq!(split_index(0), (0, None));
        assert_eq!(split_index(3), (3, None));
        assert_eq!(split_index(131_072), (0, Some(1)));
        assert_eq!(split_index((17 << 16) + 2), (2, Some(16)));
        assert_eq!(split_index(VARIABLE_DEFAULT_INSTANCE << 16), (0, None));
        assert_eq!(split_index(i32::MIN.cast_unsigned()), (0, None));
    }

    #[test]
    fn weights_map_onto_fontconfig_scale() {
        assert_eq!(fc_weight(400), 80);
        assert_eq!(fc_weight(700), 200);
    }

    #[test]
    fn an_installed_family_resolves_and_a_misspelled_one_does_not() {
        let Some(fc) = pinned("") else { return };
        let neon = fc
            .find_face(TEST_FONT_FAMILY, REGULAR_WEIGHT, false)
            .expect("installed");
        assert_eq!(family(&fc, neon), TEST_FONT_FAMILY);
        assert_eq!(fc.find_face("Monaspace Neonn", REGULAR_WEIGHT, false), None);
        assert_eq!(fc.find_face("Monaspace Neonn", 700, true), None);
    }

    #[test]
    fn a_family_with_only_bitmap_faces_is_not_installed() {
        const BDF: &str = "STARTFONT 2.1
FONT -felis-Felis Bitmap-Medium-R-Normal--16-160-75-75-C-80-ISO10646-1
SIZE 16 75 75
FONTBOUNDINGBOX 8 1 0 0
STARTPROPERTIES 3
FAMILY_NAME \"Felis Bitmap\"
FONT_ASCENT 1
FONT_DESCENT 0
ENDPROPERTIES
CHARS 1
STARTCHAR A
ENCODING 65
SWIDTH 500 0
DWIDTH 8 0
BBX 8 1 0 0
BITMAP
7E
ENDCHAR
ENDFONT
";
        let Some(fc) = pinned("") else { return };
        let dir = std::env::temp_dir().join(format!("felis-bitmap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("the font dir creates");
        std::fs::write(dir.join("felis-bitmap.bdf"), BDF).expect("the face writes");
        let added = c_path(&dir);
        // SAFETY: `config` is live; `added` outlives the call.
        let scanned =
            unsafe { (fc.lib.FcConfigAppFontAddDir)(fc.config, added.as_ptr().cast()) } != FC_FALSE;
        std::fs::remove_dir_all(&dir).ok();
        assert!(scanned);
        assert_eq!(fc.find_face("Felis Bitmap", REGULAR_WEIGHT, false), None);
        let stack = FontStack::auto_discover_in(
            &fc,
            Some("Felis Bitmap"),
            &[],
            &[],
            &StyleFaces::default(),
        )
        .expect("the monospace face loads");
        let (_, monospace) = fc.default_monospace().expect("a monospace face");
        assert!(stack.primary().display_name().starts_with(&monospace));
    }

    #[test]
    fn a_full_name_is_not_taken_for_a_family() {
        let Some(fc) = pinned("") else { return };
        assert_eq!(
            fc.find_face("Monaspace Neon Bold", REGULAR_WEIGHT, false),
            None
        );
    }

    #[test]
    fn a_config_alias_is_honored_rather_than_reported_missing() {
        let Some(fc) = pinned(
            r"<alias binding='same'><family>MyTerm</family>
              <prefer><family>Monaspace Argon</family></prefer></alias>",
        ) else {
            return;
        };
        let id = fc
            .find_face("MyTerm", REGULAR_WEIGHT, false)
            .expect("the alias maps it");
        assert_eq!(family(&fc, id), "Monaspace Argon");
        let bold = fc
            .find_face("MyTerm", 700, false)
            .expect("the alias maps bold too");
        let face = fc.resolve_face(bold).expect("recorded");
        assert!(!face.slanted);
        assert_ne!(bold, id, "bold resolves to Argon's bold file");
    }

    #[test]
    fn an_alias_to_the_face_an_unknown_name_lands_on_is_still_honored() {
        let Some(fc) = pinned("") else { return };
        let query = fc.query(UNKNOWN_FAMILY, REGULAR_WEIGHT, false);
        let landing = fc
            .face_of(fc.best_match(&query).expect("a match").raw)
            .expect("a face")
            .family;
        let Some(fc) = pinned(&format!(
            "<alias binding='same'><family>MyTerm</family>
              <prefer><family>{landing}</family></prefer></alias>"
        )) else {
            return;
        };
        let id = fc
            .find_face("MyTerm", REGULAR_WEIGHT, false)
            .expect("the alias maps it");
        assert_eq!(family(&fc, id), landing);
    }

    #[test]
    fn a_rule_that_edits_only_the_weight_still_counts_as_known() {
        let Some(fc) = pinned(
            r"<match target='pattern'><test name='family'><string>MyTerm</string></test>
              <edit name='weight' mode='assign'><const>bold</const></edit></match>",
        ) else {
            return;
        };
        let id = fc
            .find_face("MyTerm", REGULAR_WEIGHT, false)
            .expect("the rule names it");
        let unknown = fc.query(UNKNOWN_FAMILY, BOLD_WEIGHT, false);
        let unknown_bold = fc
            .face_of(fc.best_match(&unknown).expect("a match").raw)
            .expect("a face");
        let FaceSource::File(path) = fc.resolve_face(id).expect("recorded").source else {
            panic!("fontconfig faces are files");
        };
        assert_eq!(path, unknown_bold.key.path, "the weight edit applies");
    }

    #[test]
    fn the_default_monospace_follows_the_config() {
        let Some(fc) = pinned(
            r"<alias><family>monospace</family>
              <prefer><family>Monaspace Xenon</family></prefer></alias>",
        ) else {
            return;
        };
        let (_, family) = fc.default_monospace().expect("a monospace face");
        assert_eq!(family, "Monaspace Xenon");
    }

    /// fontconfig names the variable file's Regular instance; it must draw
    /// at the same position fontdb reaches by moving the default to 400.
    #[test]
    fn a_named_instance_match_draws_where_the_axis_move_does() {
        let Some(fc) = pinned("") else { return };
        let files = vec![crate::test_font_dir().join("truetype/Monaspace Neon Var.ttf")];
        let family = "Monaspace Neon Var";
        let id = fc
            .find_face(family, REGULAR_WEIGHT, false)
            .expect("installed");
        assert!(
            fc.resolve_face(id).expect("recorded").instance.is_some(),
            "fontconfig matches a named instance, not the default position"
        );
        let style = StyleFaces::default();
        let via_fc = FontStack::auto_discover_in(&fc, Some(family), &[], &[], &style)
            .expect("loads through fontconfig");
        let via_db = FontStack::discover_in_files(&files, Some(family), &[], &[], &style)
            .expect("loads through fontdb");
        assert_eq!(via_fc.primary().coords, via_db.primary().coords);
    }

    /// Sorting on the matched family's own name would drop the alias's
    /// order, and both rankings below would come out the same.
    #[test]
    fn the_cascade_follows_the_alias_that_chose_the_primary() {
        let Some(color_first) = first_fallback_is_color("Noto Color Emoji", "Noto Sans Symbols 2")
        else {
            return;
        };
        let Some(mono_first) = first_fallback_is_color("Noto Sans Symbols 2", "Noto Color Emoji")
        else {
            return;
        };
        assert!(color_first);
        assert!(!mono_first);
    }

    /// Argon NF ranks right after the primary and adds its icons.
    #[test]
    fn a_cascade_fallback_draws_its_own_familys_bold() {
        let Some(fc) = pinned(
            r"<match target='pattern'><edit name='family' mode='append' binding='strong'>
              <string>Monaspace Argon NF</string></edit></match>",
        ) else {
            return;
        };
        let stack = FontStack::auto_discover_in(
            &fc,
            Some(TEST_FONT_FAMILY),
            &[],
            &[],
            &StyleFaces::default(),
        )
        .expect("the pinned primary loads");
        let slot = (1..stack.len())
            .find(|&i| {
                stack
                    .font_at(i, FontStyle::REGULAR)
                    .display_name()
                    .starts_with("Monaspace Argon NF")
            })
            .expect("Argon NF is in the cascade");
        let bold = FontStyle {
            bold: true,
            italic: false,
        };
        assert_eq!(
            stack.font_at(slot, bold).display_name(),
            "Monaspace Argon NF Bold"
        );
    }

    /// `❤`, `✂` and `☎` default to text, `☕`, `⌚` and `☔` to emoji; both
    /// faces cover all six and the primary none.
    #[test]
    fn a_color_face_ranked_first_still_leaves_text_symbols_mono() {
        let Some(stack) = ranked_stack("Noto Color Emoji", "Noto Sans Symbols 2") else {
            return;
        };
        for text in ["\u{2764}", "\u{2702}", "\u{260E}", "\u{2615}\u{FE0E}"] {
            assert!(!drawn_in_color(&stack, text), "{text:?} must draw mono");
        }
        for text in ["\u{2615}", "\u{231A}", "\u{2614}", "\u{2764}\u{FE0F}"] {
            assert!(drawn_in_color(&stack, text), "{text:?} must draw color");
        }
    }

    #[test]
    fn a_mono_face_ranked_first_leaves_emoji_on_the_color_face() {
        let Some(stack) = ranked_stack("Noto Sans Symbols 2", "Noto Color Emoji") else {
            return;
        };
        for text in ["\u{2615}", "\u{231A}", "\u{2614}", "\u{2764}\u{FE0F}"] {
            assert!(drawn_in_color(&stack, text), "{text:?} must draw color");
        }
        for text in ["\u{2764}", "\u{2615}\u{FE0E}"] {
            assert!(!drawn_in_color(&stack, text), "{text:?} must draw mono");
        }
    }

    /// A color primary covers `✂`, but text presentation still takes the
    /// mono face that covers it too.
    #[test]
    fn a_color_primary_leaves_text_symbols_to_a_mono_fallback() {
        let Some(fc) = pinned(
            r"<match target='pattern'><edit name='family' mode='append' binding='strong'>
              <string>Noto Sans Symbols 2</string></edit></match>",
        ) else {
            return;
        };
        let stack = FontStack::auto_discover_in(
            &fc,
            Some("Noto Color Emoji"),
            &[],
            &[],
            &StyleFaces::default(),
        )
        .expect("the color primary loads");
        assert!(stack.primary().has_color_glyphs());
        assert!(!drawn_in_color(&stack, "\u{2702}"));
        assert!(drawn_in_color(&stack, "\u{2615}"));
    }

    /// The bold override has `⌨` but no Latin; bold `A` must still find
    /// a face, which only the primary family's own face provides here.
    #[test]
    fn a_styled_override_falls_back_to_the_primary_family() {
        let Some(fc) = pinned("") else { return };
        let symbols = FaceSpec::named("Noto Sans Symbols 2");
        let stack = FontStack::auto_discover_in(
            &fc,
            Some(TEST_FONT_FAMILY),
            &[],
            &[],
            &StyleFaces {
                bold: &symbols,
                ..StyleFaces::default()
            },
        )
        .expect("the pinned primary loads");
        let bold = FontStyle {
            bold: true,
            italic: false,
        };
        assert!(!stack.primary_covers('A', bold));
        let id = stack.resolve_styled_index('A', bold);
        assert_ne!(id, 0);
        assert!(stack.font_at(id, bold).has_glyph('A'));
    }

    #[test]
    fn an_explicit_fallback_replaces_the_cascade_and_its_mono_first_rule() {
        let Some(fc) = pinned("") else { return };
        let stack = FontStack::auto_discover_in(
            &fc,
            Some(TEST_FONT_FAMILY),
            &[],
            &[
                FaceSpec::named("Noto Color Emoji"),
                FaceSpec::named("Noto Sans Symbols 2"),
            ],
            &StyleFaces::default(),
        )
        .expect("the pinned primary loads");
        assert_eq!(stack.len(), 3);
        assert!(
            drawn_in_color(&stack, "\u{2764}"),
            "the declared color face covers it first"
        );
    }
}

//! Face discovery through the host's libfontconfig, so the faces drawn are
//! the ones `fc-match` names among the formats the loader parses, and the
//! user's `fonts.conf` rules apply.

use std::{
    cell::{OnceCell, RefCell},
    collections::{HashMap, HashSet},
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

use tracing::info;

use super::{
    CascadeFace, FaceProvider, FaceSource, LoadedAs, REGULAR_WEIGHT, ResolvedFace,
    ascii_is_fixed_pitch,
};
use crate::{FaceStyle, InstalledFamily, ShapingError, map_face_file};

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

/// The generic names fontconfig's stock `48-guessfamily.conf` routes a
/// family name containing one of them to. A name not installed that
/// lands where one of these does was guessed, not mapped by an alias.
const GENERIC_FAMILIES: [&str; 15] = [
    "sans-serif",
    "sans",
    "serif",
    "monospace",
    "mono",
    "cursive",
    "fantasy",
    "fangsong",
    "emoji",
    "math",
    "system-ui",
    "ui-sans-serif",
    "ui-serif",
    "ui-monospace",
    "ui-rounded",
];

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

/// fontconfig's map between the OpenType weight scale and its own
/// (`fcweight.c`), interpolated linearly between these points. Ported
/// rather than called: `FcWeightFromOpenTypeDouble` is missing from
/// older libraries, and a missing symbol would stop every window opening.
const WEIGHT_SCALE: [(f64, f64); 13] = [
    (0.0, 0.0),
    (100.0, 0.0),
    (200.0, 40.0),
    (300.0, 50.0),
    (350.0, 55.0),
    (380.0, 75.0),
    (400.0, 80.0),
    (500.0, 100.0),
    (600.0, 180.0),
    (700.0, 200.0),
    (800.0, 205.0),
    (900.0, 210.0),
    (1000.0, 215.0),
];

/// Whether `name` is one of the generic names, compared as fontconfig
/// compares family names: ignoring ASCII case and spaces.
fn is_generic_name(name: &str) -> bool {
    let fold = |s: &str| {
        s.bytes()
            .filter(|&b| b != b' ')
            .map(|b| b.to_ascii_lowercase())
            .collect::<Vec<u8>>()
    };
    let name = fold(name);
    GENERIC_FAMILIES.iter().any(|generic| fold(generic) == name)
}

/// Interpolates `x` along `points` read as `(from, to)` pairs; `None`
/// outside them.
fn interpolate(points: impl Iterator<Item = (f64, f64)> + Clone, x: f64) -> Option<f64> {
    let (first, _) = points.clone().next()?;
    if x.is_nan() || x < first {
        return None;
    }
    let mut prev = None;
    for (from, to) in points {
        if x <= from {
            return Some(match prev {
                Some((pf, pt)) if from > pf => pt + (to - pt) * (x - pf) / (from - pf),
                _ => to,
            });
        }
        prev = Some((from, to));
    }
    None
}

fn weight_from_opentype(weight: f64) -> f64 {
    interpolate(WEIGHT_SCALE.iter().copied(), weight.min(1000.0)).unwrap_or(-1.0)
}

fn weight_to_opentype(weight: f64) -> f64 {
    // fontconfig skips the flat (0, 100) segment going back, so its own
    // weight 0 reads as OpenType 100.
    interpolate(WEIGHT_SCALE[1..].iter().map(|&(ot, fc)| (fc, ot)), weight).unwrap_or(-1.0)
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
    /// The families the generic names match, keyed by (weight, italic)
    /// since a rule can test either.
    generic: RefCell<HashMap<(u16, bool), Vec<String>>>,
    /// Names already logged as resolved by an alias, so each style's
    /// lookup of the same name does not log it again.
    aliased: RefCell<HashSet<String>>,
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
        // Before the scan, so a `target="scan"` rule edits the faces.
        if let Some(rules) = rules_file {
            let rules = c_path(rules);
            // SAFETY: `config` is live; `rules` outlives the call.
            if unsafe { (lib.FcConfigParseAndLoad)(config, rules.as_ptr().cast(), FC_TRUE) }
                == FC_FALSE
            {
                return Err(fail("FcConfigParseAndLoad"));
            }
        }
        let dir = c_path(font_dir);
        // SAFETY: as above.
        if unsafe { (lib.FcConfigAppFontAddDir)(config, dir.as_ptr().cast()) } == FC_FALSE {
            return Err(fail("FcConfigAppFontAddDir"));
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
            generic: RefCell::new(HashMap::new()),
            aliased: RefCell::new(HashSet::new()),
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
            (lib.FcPatternAddDouble)(raw, FC_WEIGHT.as_ptr(), Self::fc_weight(weight));
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

    fn is_generic_match(&self, family: &str, weight: u16, italic: bool) -> bool {
        let mut generic = self.generic.borrow_mut();
        generic
            .entry((weight, italic))
            .or_insert_with(|| {
                GENERIC_FAMILIES
                    .iter()
                    .filter_map(|name| {
                        let matched = self.best_match(&self.query(name, weight, italic))?;
                        Some(self.face_of(matched.raw)?.family)
                    })
                    .collect()
            })
            .iter()
            .any(|generic| generic == family)
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

    /// Every family name a loadable face carries, one per name fontconfig
    /// matches as the same family (ignoring case and spaces, its
    /// `FcStrCmpIgnoreBlanksAndCase`), described by the faces of all the
    /// names merged. Of the spellings that compare equal, the bytewise
    /// smallest is kept.
    pub(crate) fn installed_families(&self) -> Vec<InstalledFamily> {
        let sets = self.families();
        let mut names: Vec<CString> = sets
            .keys()
            .filter_map(|name| CString::new(name.clone()).ok())
            .collect();
        names.sort();
        let blankless: Vec<CString> = names
            .iter()
            .map(|name| {
                let kept = name.as_bytes().iter().copied().filter(|&b| b != b' ');
                CString::new(kept.collect::<Vec<u8>>()).unwrap_or_default()
            })
            .collect();
        let compare = |a: usize, b: usize| {
            // SAFETY: both strings are NUL-terminated and outlive the call.
            let order = unsafe {
                (self.lib.FcStrCmpIgnoreCase)(
                    blankless[a].as_ptr().cast(),
                    blankless[b].as_ptr().cast(),
                )
            };
            order.cmp(&0)
        };
        let mut order: Vec<usize> = (0..names.len()).collect();
        order.sort_by(|&a, &b| compare(a, b));
        let mut pitches: HashMap<FaceKey, bool> = HashMap::new();
        let mut families: Vec<InstalledFamily> = Vec::new();
        let mut kept = None;
        for at in order {
            let family = match families.last_mut() {
                Some(family) if kept.is_some_and(|kept| compare(at, kept).is_eq()) => family,
                _ => {
                    kept = Some(at);
                    families.push(InstalledFamily::new(
                        names[at].to_string_lossy().into_owned(),
                    ));
                    let Some(family) = families.last_mut() else {
                        continue;
                    };
                    family
                }
            };
            for pattern in sets[names[at].as_bytes()].patterns() {
                let Some(face) = self.face_of(pattern) else {
                    continue;
                };
                let monospace = *pitches.entry(face.key).or_insert_with_key(|key| {
                    map_face_file(&key.path)
                        .is_ok_and(|data| ascii_is_fixed_pitch(&data, split_index(key.index).0))
                });
                match self.style_of(pattern) {
                    Some(style) => family.add_face(monospace, style),
                    None => family.monospace &= monospace,
                }
            }
        }
        families
    }

    /// `None` for a variable face's own pattern, whose weight is a range;
    /// its named instances are listed as faces of their own.
    fn style_of(&self, pattern: *mut FcPattern) -> Option<FaceStyle> {
        Some(FaceStyle {
            italic: self
                .integer(pattern, FC_SLANT)
                .is_some_and(|s| s != FC_SLANT_ROMAN),
            weight: Self::opentype_weight(self.double(pattern, FC_WEIGHT)?)?,
        })
    }

    fn fc_weight(weight: u16) -> f64 {
        weight_from_opentype(f64::from(weight))
    }

    /// `None` for a weight off fontconfig's scale, which it maps to -1.
    fn opentype_weight(weight: f64) -> Option<u16> {
        let opentype = weight_to_opentype(weight);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped into 1..=1000 first"
        )]
        (opentype >= 1.0).then(|| opentype.round().min(1000.0) as u16)
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

    /// An integer value reads as a double too.
    fn double(&self, pattern: *mut FcPattern, object: &CStr) -> Option<f64> {
        let mut value = 0.0;
        // SAFETY: `pattern` is live.
        let found =
            unsafe { (self.lib.FcPatternGetDouble)(pattern, object.as_ptr(), 0, &raw mut value) };
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

    /// A name no installed face carries is accepted only when it lands on
    /// a family no generic name matches, so an `<alias>` in `fonts.conf`
    /// is honored while a typo the stock rules route by a substring is
    /// not (`docs/explanation/rendering/text-shaping.md` "Linux faces").
    fn find_face(&self, family: &str, weight: u16, italic: bool) -> Option<usize> {
        let query = self.query(family, weight, italic);
        let found = self.face_of(self.best_match(&query)?.raw)?;
        if !self.is_installed(family) && !is_generic_name(family) {
            if self.is_generic_match(&found.family, weight, italic) {
                return None;
            }
            if self.aliased.borrow_mut().insert(family.to_owned()) {
                info!(
                    family,
                    resolved = found.family,
                    "font family resolved by fontconfig alias"
                );
            }
        }
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

    fn default_monospace(&self, weight: u16) -> Result<(usize, String), ShapingError> {
        let query = self.query("monospace", weight, false);
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
    use crate::{FaceSpec, Font, FontStack, FontStyle, Shaper, StyleFaces, TEST_FONT_FAMILY};

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
    fn the_ported_weight_map_matches_the_host_fontconfig() {
        // SAFETY: loading libfontconfig runs only its initializers, and
        // both symbols are `double (double)` in `fontconfig.h`.
        let lib = unsafe { libloading::Library::new(SONAME) }.expect("libfontconfig");
        let symbol = |name: &[u8]| {
            // SAFETY: as above; `lib` outlives every call below.
            *unsafe { lib.get::<unsafe extern "C" fn(f64) -> f64>(name) }.expect("symbol")
        };
        let from = symbol(b"FcWeightFromOpenTypeDouble\0");
        let to = symbol(b"FcWeightToOpenTypeDouble\0");
        for ot in [
            -5.0, 0.0, 50.0, 100.0, 150.0, 350.0, 390.0, 450.0, 530.0, 999.0, 1000.0, 1200.0,
        ] {
            // SAFETY: pure arithmetic functions of their argument.
            assert_eq!(weight_from_opentype(ot), unsafe { from(ot) }, "from {ot}");
        }
        for fc in [-5.0, 0.0, 20.0, 55.0, 77.0, 80.0, 90.0, 190.0, 215.0, 300.0] {
            // SAFETY: as above.
            assert_eq!(weight_to_opentype(fc), unsafe { to(fc) }, "to {fc}");
        }
    }

    #[test]
    fn weights_map_continuously_onto_fontconfig_scale() {
        assert_eq!(Fontconfig::fc_weight(400).to_bits(), 80.0_f64.to_bits());
        assert_eq!(Fontconfig::fc_weight(700).to_bits(), 200.0_f64.to_bits());
        let between = Fontconfig::fc_weight(450);
        assert!(80.0 < between && between < 100.0, "{between}");
        for weight in [100, 350, 400, 450, 530, 700, 900] {
            assert_eq!(
                Fontconfig::opentype_weight(Fontconfig::fc_weight(weight)),
                Some(weight)
            );
        }
        assert_eq!(Fontconfig::opentype_weight(-5.0), None);
    }

    #[test]
    fn a_weight_between_static_faces_resolves_to_a_neighbor() {
        let Some(fc) = pinned("") else { return };
        let file_at = |weight| {
            let id = fc
                .find_face(TEST_FONT_FAMILY, weight, false)
                .expect("installed");
            let FaceSource::File(path) = fc.resolve_face(id).expect("recorded").source else {
                panic!("fontconfig faces are files");
            };
            path.file_name()
                .expect("a file")
                .to_string_lossy()
                .into_owned()
        };
        assert_eq!(file_at(300), "MonaspaceNeon-Light.otf");
        assert_eq!(file_at(500), "MonaspaceNeon-Medium.otf");
        let between = file_at(350);
        assert!(
            ["MonaspaceNeon-Light.otf", "MonaspaceNeon-Regular.otf"].contains(&between.as_str()),
            "{between}"
        );
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

    /// A BDF face of family `Felis Bitmap`, which fontconfig lists and
    /// the loader cannot parse.
    fn add_bitmap_face(fc: &Fontconfig) {
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
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "felis-bitmap-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("the font dir creates");
        std::fs::write(dir.join("felis-bitmap.bdf"), BDF).expect("the face writes");
        let added = c_path(&dir);
        // SAFETY: `config` is live; `added` outlives the call.
        let scanned =
            unsafe { (fc.lib.FcConfigAppFontAddDir)(fc.config, added.as_ptr().cast()) } != FC_FALSE;
        std::fs::remove_dir_all(&dir).ok();
        assert!(scanned);
    }

    #[test]
    fn the_family_list_names_loadable_faces_and_drops_bitmap_ones() {
        let Some(fc) = pinned("") else { return };
        add_bitmap_face(&fc);
        // SAFETY: `config` is live; the set stays owned by it.
        let scanned = ManuallyDrop::new(FontSet {
            lib: fc.lib,
            raw: unsafe { (fc.lib.FcConfigGetFonts)(fc.config, FcSetApplication) },
        });
        assert!(
            scanned
                .patterns()
                .any(|p| fc.string(p, FC_FAMILY) == Some(c"Felis Bitmap")),
            "fontconfig itself lists the bitmap face"
        );
        let names: Vec<String> = fc
            .installed_families()
            .into_iter()
            .map(|family| family.name)
            .collect();
        assert!(names.iter().any(|n| n == TEST_FONT_FAMILY), "{names:?}");
        assert!(!names.iter().any(|n| n == "Felis Bitmap"), "{names:?}");
        for name in &names {
            assert!(
                fc.find_face(name, REGULAR_WEIGHT, false).is_some(),
                "{name} is listed but not accepted"
            );
        }
    }

    #[test]
    fn names_fontconfig_matches_as_one_family_are_listed_once() {
        let Some(fc) = pinned(
            r"<match target='scan'><edit name='family' mode='append'>
              <string>monaspace  NEON</string></edit></match>",
        ) else {
            return;
        };
        let families = fc.installed_families();
        let squashed = |n: &str| n.replace(' ', "").to_lowercase();
        let neon: Vec<&InstalledFamily> = families
            .iter()
            .filter(|family| squashed(&family.name) == "monaspaceneon")
            .collect();
        assert_eq!(neon.len(), 1, "{neon:?}");
        assert_eq!(
            neon[0].name, "Monaspace Neon",
            "the bytewise smallest spelling"
        );
    }

    #[test]
    fn fontconfig_reports_each_familys_pitch_and_styles() {
        let Some(fc) = pinned("") else { return };
        let families = fc.installed_families();
        let named = |name: &str| {
            families
                .iter()
                .find(|family| family.name == name)
                .unwrap_or_else(|| panic!("{name} is installed"))
        };
        let argon = named("Monaspace Argon");
        assert!(argon.monospace);
        for (italic, weight) in [(false, 400), (false, 700), (true, 400), (true, 700)] {
            let style = FaceStyle { italic, weight };
            assert!(argon.styles.contains(&style), "{:?}", argon.styles);
        }
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

    #[test]
    fn a_family_with_only_bitmap_faces_is_not_installed() {
        let Some(fc) = pinned("") else { return };
        add_bitmap_face(&fc);
        assert_eq!(fc.find_face("Felis Bitmap", REGULAR_WEIGHT, false), None);
        let stack = FontStack::auto_discover_in(
            &fc,
            Some("Felis Bitmap"),
            &[],
            &[],
            &StyleFaces::default(),
        )
        .expect("the monospace face loads");
        let (_, monospace) = fc
            .default_monospace(REGULAR_WEIGHT)
            .expect("a monospace face");
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
              <prefer><family>Monaspace Neon</family></prefer></alias>",
        ) else {
            return;
        };
        let id = fc
            .find_face("MyTerm", REGULAR_WEIGHT, false)
            .expect("the alias maps it");
        assert_eq!(family(&fc, id), "Monaspace Neon");
        let bold = fc
            .find_face("MyTerm", 700, false)
            .expect("the alias maps bold too");
        let face = fc.resolve_face(bold).expect("recorded");
        assert!(!face.slanted);
        assert_ne!(bold, id, "bold resolves to Neon's bold file");
    }

    /// The stock `48-guessfamily.conf` shape: a name containing `mono`
    /// gets `monospace` appended, which prefers Monaspace Xenon.
    const GUESS_MONO: &str = r"<match target='pattern'>
          <test name='family' compare='contains'><string>mono</string></test>
          <edit name='family' mode='append_last'><string>monospace</string></edit></match>
        <alias><family>monospace</family><prefer><family>Monaspace Xenon</family></prefer></alias>";

    #[test]
    fn a_name_a_rule_routes_to_a_generic_by_substring_is_reported_missing() {
        let Some(fc) = pinned(GUESS_MONO) else { return };
        for (weight, italic) in [(REGULAR_WEIGHT, false), (BOLD_WEIGHT, true)] {
            assert_eq!(fc.find_face("Qwxyz Mono", weight, italic), None);
        }
    }

    #[test]
    fn a_generic_name_resolves_to_its_generic_face() {
        let Some(fc) = pinned(GUESS_MONO) else { return };
        let id = fc
            .find_face("Mono Space", REGULAR_WEIGHT, false)
            .expect("a generic name is accepted");
        assert_eq!(family(&fc, id), "Monaspace Xenon");
    }

    #[test]
    fn an_alias_to_the_face_a_generic_name_matches_is_reported_missing() {
        let Some(fc) = pinned(&format!(
            "{GUESS_MONO}<alias binding='same'><family>MyTerm</family>
              <prefer><family>Monaspace Xenon</family></prefer></alias>"
        )) else {
            return;
        };
        assert_eq!(fc.find_face("MyTerm", REGULAR_WEIGHT, false), None);
    }

    #[test]
    fn the_default_monospace_follows_the_config() {
        let Some(fc) = pinned(
            r"<alias><family>monospace</family>
              <prefer><family>Monaspace Xenon</family></prefer></alias>",
        ) else {
            return;
        };
        let (_, family) = fc
            .default_monospace(REGULAR_WEIGHT)
            .expect("a monospace face");
        assert_eq!(family, "Monaspace Xenon");
    }

    /// fontconfig names the variable file's Regular instance; it must draw
    /// at the same position a file set reaches by moving the default to 400.
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
        let via_files = FontStack::discover_in_files(&files, Some(family), &[], &[], &style)
            .expect("loads from the file");
        assert_eq!(via_fc.primary().coords, via_files.primary().coords);
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
    fn an_explicit_fallback_comes_before_the_cascade_and_outside_its_mono_first_rule() {
        let Some(fc) = pinned("") else { return };
        let explicit = [
            FaceSpec::named("Noto Color Emoji"),
            FaceSpec::named("Noto Sans Symbols 2"),
        ];
        let stack = FontStack::auto_discover_in(
            &fc,
            Some(TEST_FONT_FAMILY),
            &[],
            &explicit,
            &StyleFaces::default(),
        )
        .expect("the pinned primary loads");
        let names = stack.describe().fallbacks;
        assert!(names[0].starts_with("Noto Color Emoji"));
        assert!(names[1].starts_with("Noto Sans Symbols 2"));
        assert!(
            names[2..].iter().all(|n| *n != names[0] && *n != names[1]),
            "the cascade does not repeat a declared face"
        );
        assert!(
            drawn_in_color(&stack, "\u{2764}"),
            "the declared color face covers it first"
        );
        let han = stack.resolve('\u{845B}', FontStyle::REGULAR);
        assert!(
            han.has_glyph('\u{845B}'),
            "the cascade covers what the declared faces miss"
        );
    }

    /// Both CJK subsets cover `葛`, so only the query's language can put
    /// one ahead. The subsets are too small for fontconfig to derive a
    /// language from, so the rules tag them as a full face would be.
    #[test]
    fn the_cjk_fallback_follows_the_query_language() {
        const JP: &str = "Noto Sans CJK JP";
        const SC: &str = "Noto Sans CJK SC";
        let rules = format!(
            r"<match target='scan'><test name='family'><string>{JP}</string></test>
              <edit name='lang' mode='assign'><langset><string>ja</string></langset></edit></match>
              <match target='scan'><test name='family'><string>{SC}</string></test>
              <edit name='lang' mode='assign'><langset><string>zh-cn</string></langset></edit></match>"
        );
        for (lang, expected) in [("ja", JP), ("zh-cn", SC)] {
            let Some(fc) = pinned_config(&rules, lang) else {
                return;
            };
            for name in [JP, SC] {
                let id = fc
                    .find_face(name, REGULAR_WEIGHT, false)
                    .unwrap_or_else(|| panic!("{name} is installed"));
                let font = Font::load_face_id(&fc, id).expect("the subset loads");
                assert!(font.has_glyph('葛'), "{name} covers 葛");
            }
            let stack = FontStack::auto_discover_in(
                &fc,
                Some(TEST_FONT_FAMILY),
                &[],
                &[],
                &StyleFaces::default(),
            )
            .expect("the pinned primary loads");
            let han = stack.resolve('葛', FontStyle::REGULAR).display_name();
            assert!(han.starts_with(expected), "{lang}: {han}");
        }
    }
}

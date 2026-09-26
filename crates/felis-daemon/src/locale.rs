//! Filling `LANG` for a create whose resolved environment names no
//! locale: launchd gives a `.app`-launched process none, so the child
//! encodes text for the OS as the region's legacy charset rather than
//! UTF-8. Fill, never overwrite (`docs/explanation/security-model.md`
//! "Process and environment boundary").
use crate::child_env::{self, EnvEntry};

/// `LC_CTYPE` alone is enough to settle the encoding, and `LC_ALL`
/// outranks `LANG`, so a `LANG`-only check would push a `LANG` onto a
/// user who deliberately set just one of the other two.
const LOCALE_KEYS: [&str; 3] = ["LANG", "LC_ALL", "LC_CTYPE"];

/// The directory macOS's `setlocale` consults; a candidate absent from
/// it is one `setlocale` would reject.
#[cfg(target_os = "macos")]
const LOCALE_DIR: &str = "/usr/share/locale";

#[cfg(any(target_os = "macos", test))]
const FALLBACK_LANG: &str = "en_US.UTF-8";

/// The `LANG` to stamp into a create whose base (`Some`) or, for a
/// birth-env create, whose daemon environment (`None`) names no
/// locale. Always `None` off macOS.
#[must_use]
pub(crate) fn fill_lang(base: Option<&[EnvEntry]>, windows: bool) -> Option<String> {
    let named = match base {
        Some(entries) => base_has_locale(entries, windows),
        None => daemon_env_has_locale(),
    };
    if named { None } else { system_lang() }
}

#[must_use]
fn base_has_locale(entries: &[EnvEntry], windows: bool) -> bool {
    LOCALE_KEYS
        .iter()
        .any(|key| child_env::lookup(entries, key, windows).is_some_and(|value| !value.is_empty()))
}

#[must_use]
fn daemon_env_has_locale() -> bool {
    LOCALE_KEYS
        .iter()
        .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
}

#[cfg(not(target_os = "macos"))]
#[must_use]
const fn system_lang() -> Option<String> {
    None
}

/// The current region as a locale name, queried per create: the daemon
/// outlives a change in System Settings.
#[cfg(target_os = "macos")]
#[must_use]
fn system_lang() -> Option<String> {
    use objc2_core_foundation::{CFLocale, CFString};

    // SAFETY: `kCFLocaleLanguageCode` and `kCFLocaleCountryCode` are
    // immutable `CFStringRef` constants exported by CoreFoundation and
    // initialized before any client code runs; reading them yields a
    // `&'static CFLocaleKey` that is never written to. The `unsafe` is
    // only that these are `extern` statics.
    #[allow(unsafe_code)]
    let (language_key, country_key) = unsafe {
        (
            objc2_core_foundation::kCFLocaleLanguageCode,
            objc2_core_foundation::kCFLocaleCountryCode,
        )
    };

    let locale = CFLocale::current()?;
    let read = |key| {
        locale
            .value(key)
            .and_then(|value| value.downcast::<CFString>().ok())
            .map(|value| value.to_string())
    };
    // Not `kCFLocaleIdentifier`: newer macOS appends `@currency=…` and
    // other collator metadata to it, which is not a locale name.
    Some(candidate(
        read(language_key).as_deref(),
        read(country_key).as_deref(),
        |name| std::path::Path::new(LOCALE_DIR).join(name).is_dir(),
    ))
}

#[cfg(any(target_os = "macos", test))]
#[must_use]
fn candidate(
    language: Option<&str>,
    country: Option<&str>,
    exists: impl Fn(&str) -> bool,
) -> String {
    let Some((language, country)) = language.zip(country) else {
        return FALLBACK_LANG.to_string();
    };
    let candidate = format!("{language}_{country}.UTF-8");
    if exists(&candidate) {
        candidate
    } else {
        FALLBACK_LANG.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, value: &str) -> EnvEntry {
        (name.as_bytes().to_vec(), value.as_bytes().to_vec())
    }

    fn windows_entry(name: &str, value: &str) -> EnvEntry {
        let units = |text: &str| text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        (units(name), units(value))
    }

    #[test]
    fn a_validated_language_country_pair_becomes_the_candidate() {
        assert_eq!(
            candidate(Some("ja"), Some("JP"), |name| name == "ja_JP.UTF-8"),
            "ja_JP.UTF-8"
        );
    }

    #[test]
    fn a_missing_country_code_falls_back() {
        assert_eq!(candidate(Some("ja"), None, |_| true), "en_US.UTF-8");
    }

    #[test]
    fn a_missing_language_code_falls_back() {
        assert_eq!(candidate(None, Some("JP"), |_| true), "en_US.UTF-8");
    }

    #[test]
    fn a_pair_the_system_does_not_install_falls_back() {
        assert_eq!(candidate(Some("xx"), Some("YY"), |_| false), "en_US.UTF-8");
    }

    #[test]
    fn a_base_with_no_locale_keys_is_unfilled() {
        assert!(!base_has_locale(&[entry("PATH", "/usr/bin")], false));
    }

    #[test]
    fn an_empty_lang_counts_as_unset() {
        assert!(!base_has_locale(&[entry("LANG", "")], false));
    }

    #[test]
    fn a_set_lang_counts_as_named() {
        assert!(base_has_locale(&[entry("LANG", "ja_JP.UTF-8")], false));
    }

    #[test]
    fn lc_all_alone_counts_as_named() {
        assert!(base_has_locale(&[entry("LC_ALL", "C")], false));
    }

    #[test]
    fn lc_ctype_alone_counts_as_named() {
        assert!(base_has_locale(&[entry("LC_CTYPE", "UTF-8")], false));
    }

    #[test]
    fn a_lowercase_name_counts_as_named_under_windows_folding() {
        assert!(base_has_locale(
            &[windows_entry("lang", "ja_JP.UTF-8")],
            true
        ));
        assert!(!base_has_locale(
            &[windows_entry("lang", "ja_JP.UTF-8")],
            false
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_system_lang_names_an_installed_utf8_locale() {
        let lang = system_lang().expect("macOS always yields a candidate");
        assert!(lang.ends_with(".UTF-8"), "got: {lang}");
        assert!(
            std::path::Path::new(LOCALE_DIR).join(&lang).is_dir(),
            "candidate is not installed: {lang}"
        );
    }
}

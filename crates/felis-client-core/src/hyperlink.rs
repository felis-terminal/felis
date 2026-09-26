//! `ActivationTarget`: the typed boundary an OSC 8 URI crosses before a
//! platform launcher ever sees it (`docs/explanation/security-model.md`).
//! Re-validates the allowlist and closes the bidi-control gap that passes
//! the grid-side C0/DEL filter untouched.

use felis_grid::LinkText;

/// One of the four schemes REQ-910 allows. Kept distinct from the raw
/// scheme string so a log line can carry the class without the URI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemeClass {
    Http,
    Https,
    Mailto,
    File,
}

/// Why `ActivationTarget::parse` refused a string. Carries no part of the
/// URI: a rejection is exactly what a log line at the activation site may
/// hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationRejection {
    /// No `scheme:` prefix at all.
    NoScheme,
    /// A `scheme:` prefix outside the REQ-910 allowlist.
    SchemeDenied,
    /// An interior NUL. Kept distinct from `ControlChar` (NUL is also a
    /// control) because truncation is the specific failure it causes at
    /// an OS launcher.
    InteriorNul,
    /// A C0, DEL, or C1 control byte.
    ControlChar,
    /// A bidi-reordering codepoint (see `is_bidi_control`).
    BidiControl,
    /// Longer than `LinkText::CAP`, the parser's own OSC body limit.
    TooLong,
}

/// A URI that has passed the activation-time re-check: allowlisted
/// scheme, no NUL/control/bidi codepoints, within the parser's length
/// cap. The only way to get one; nothing else in this crate builds an
/// `ActivationTarget` from an unchecked string.
#[derive(Clone, PartialEq, Eq)]
pub struct ActivationTarget {
    uri: String,
    scheme: SchemeClass,
}

impl ActivationTarget {
    /// Re-validates `uri` from scratch rather than trusting whatever
    /// produced it. Order: scheme allowlist, then NUL, then any other
    /// control character, then bidi controls, then the length cap;
    /// each check reports the one violation a caller needs to log.
    pub fn parse(uri: &str) -> Result<Self, ActivationRejection> {
        let scheme = scheme_of(uri)?;
        if uri.contains('\0') {
            return Err(ActivationRejection::InteriorNul);
        }
        if uri.chars().any(char::is_control) {
            return Err(ActivationRejection::ControlChar);
        }
        if uri.chars().any(is_bidi_control) {
            return Err(ActivationRejection::BidiControl);
        }
        if uri.len() > LinkText::CAP {
            return Err(ActivationRejection::TooLong);
        }
        Ok(Self {
            uri: uri.to_owned(),
            scheme,
        })
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.uri
    }

    #[must_use]
    pub const fn scheme(&self) -> SchemeClass {
        self.scheme
    }

    /// A display-safe preview clipped to `max_chars`. `parse` has already
    /// refused every control and bidi codepoint, so clipping is the only
    /// transform needed here: the preview is the validated string
    /// itself, not a re-sanitized copy (asserted by
    /// `preview_never_contains_forbidden_chars` below).
    #[must_use]
    pub fn preview(&self, max_chars: usize) -> String {
        let mut chars = self.uri.chars();
        let head: String = chars.by_ref().take(max_chars).collect();
        if chars.next().is_none() {
            head
        } else {
            format!("{head}…")
        }
    }

    /// The only pair of facts a log line may carry about an activated
    /// target: which scheme class, and how long the URI was in bytes.
    #[must_use]
    pub const fn log_fields(&self) -> (SchemeClass, usize) {
        (self.scheme, self.uri.len())
    }
}

/// Redacted rather than derived: `?target` in a tracing field, a panic
/// message, or an `assert_eq!` failure would otherwise print the
/// producer-controlled URI that the activation boundary exists to keep
/// out of diagnostics. `log_fields` is the whole permitted set, and this
/// prints exactly that.
impl std::fmt::Debug for ActivationTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivationTarget")
            .field("scheme", &self.scheme)
            .field("len", &self.uri.len())
            .finish_non_exhaustive()
    }
}

/// REQ-910 / `docs/explanation/security-model.md` "OSC 8 hyperlinks and
/// OSC 7 CWD": mirrors `felis_grid::osc8_scheme_allowed` so the client-side boundary
/// enforces the identical allowlist rather than a second, driftable copy
/// of the same four names.
fn scheme_of(uri: &str) -> Result<SchemeClass, ActivationRejection> {
    let (scheme, _) = uri.split_once(':').ok_or(ActivationRejection::NoScheme)?;
    if scheme.eq_ignore_ascii_case("http") {
        Ok(SchemeClass::Http)
    } else if scheme.eq_ignore_ascii_case("https") {
        Ok(SchemeClass::Https)
    } else if scheme.eq_ignore_ascii_case("mailto") {
        Ok(SchemeClass::Mailto)
    } else if scheme.eq_ignore_ascii_case("file") {
        Ok(SchemeClass::File)
    } else {
        Err(ActivationRejection::SchemeDenied)
    }
}

/// U+202A–U+202E (embedding/override), U+2066–U+2069 (isolates), U+200E/F
/// (marks), U+061C (Arabic letter mark). Rejected rather than stripped:
/// a URI carrying one is hostile by construction (RFC 3986 URIs are
/// ASCII after IRI mapping), and stripping would activate a target the
/// user never saw.
const fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}'
    )
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn accepts_the_four_allowlisted_schemes_case_insensitively() {
        assert_eq!(
            ActivationTarget::parse("HTTPS://example.com")
                .expect("HTTPS is allowlisted")
                .scheme(),
            SchemeClass::Https
        );
        assert_eq!(
            ActivationTarget::parse("mailto:a@example.com")
                .expect("mailto is allowlisted")
                .scheme(),
            SchemeClass::Mailto
        );
        assert!(ActivationTarget::parse("http://example.com").is_ok());
        assert!(ActivationTarget::parse("file:///etc/hosts").is_ok());
    }

    #[test]
    fn rejects_javascript_and_data_schemes() {
        // The issue's headline acceptance criterion: these must be
        // refused at activation even if a future grid path stored them.
        assert_eq!(
            ActivationTarget::parse("javascript:alert(1)"),
            Err(ActivationRejection::SchemeDenied)
        );
        assert_eq!(
            ActivationTarget::parse("data:text/html,<script>alert(1)</script>"),
            Err(ActivationRejection::SchemeDenied)
        );
        assert_eq!(
            ActivationTarget::parse("JAVASCRIPT:alert(1)"),
            Err(ActivationRejection::SchemeDenied)
        );
        assert_eq!(
            ActivationTarget::parse("vbscript:msgbox(1)"),
            Err(ActivationRejection::SchemeDenied)
        );
    }

    #[test]
    fn rejects_a_uri_with_no_scheme() {
        assert_eq!(
            ActivationTarget::parse("not-a-uri"),
            Err(ActivationRejection::NoScheme)
        );
    }

    #[test]
    fn rejects_an_interior_nul_distinctly_from_other_control_chars() {
        assert_eq!(
            ActivationTarget::parse("https://example.com/\0evil"),
            Err(ActivationRejection::InteriorNul)
        );
    }

    #[test]
    fn rejects_escape_and_next_line_control_chars() {
        assert_eq!(
            ActivationTarget::parse("https://example.com/\x1b[31m"),
            Err(ActivationRejection::ControlChar)
        );
        // U+0085 NEL: a C1 control, not ASCII, so it only fails the
        // `char::is_control` check, not a byte-range one.
        assert_eq!(
            ActivationTarget::parse("https://example.com/\u{85}"),
            Err(ActivationRejection::ControlChar)
        );
    }

    #[test]
    fn rejects_bidi_override_and_isolate_controls() {
        assert_eq!(
            ActivationTarget::parse("https://example.com/\u{202e}gpj.exe"),
            Err(ActivationRejection::BidiControl)
        );
        assert_eq!(
            ActivationTarget::parse("https://example.com/\u{2066}x\u{2069}"),
            Err(ActivationRejection::BidiControl)
        );
    }

    #[test]
    fn rejects_a_uri_past_the_link_text_cap() {
        let uri = format!("https://example.com/{}", "x".repeat(LinkText::CAP));
        assert_eq!(
            ActivationTarget::parse(&uri),
            Err(ActivationRejection::TooLong)
        );
    }

    #[test]
    fn accepts_a_uri_exactly_at_the_cap() {
        let path_len = LinkText::CAP - "https://e/".len();
        let uri = format!("https://e/{}", "x".repeat(path_len));
        assert_eq!(uri.len(), LinkText::CAP);
        assert!(ActivationTarget::parse(&uri).is_ok());
    }

    #[test]
    fn preview_clips_at_the_char_count_and_marks_truncation() {
        let target = ActivationTarget::parse("https://example.com/aaaaaaaaaa").unwrap();
        assert_eq!(target.preview(64), "https://example.com/aaaaaaaaaa");
        let clipped = target.preview(8);
        assert_eq!(clipped, "https://…");
        assert_eq!(clipped.chars().count(), 9);
    }

    #[test]
    fn log_fields_carries_only_scheme_class_and_byte_length() {
        let target = ActivationTarget::parse("https://example.com/secret-path").unwrap();
        let (scheme, len) = target.log_fields();
        assert_eq!(scheme, SchemeClass::Https);
        assert_eq!(len, "https://example.com/secret-path".len());
    }

    /// A `?target` tracing field is the way the URI would most plausibly
    /// come back into a log line after the call sites were cleaned up.
    #[test]
    fn debug_formatting_carries_no_part_of_the_uri() {
        let target = ActivationTarget::parse("https://example.com/secret-path").unwrap();
        let rendered = format!("{target:?}");
        assert!(!rendered.contains("example.com"), "{rendered}");
        assert!(!rendered.contains("secret-path"), "{rendered}");
        assert!(rendered.contains("Https"), "{rendered}");
        assert!(
            rendered.contains(&"https://example.com/secret-path".len().to_string()),
            "{rendered}"
        );
    }

    /// Whatever `parse` accepts must yield a preview with no control or
    /// bidi codepoint at any clip width. Asserts non-zero accepted cases
    /// so an overly strict parser cannot silently pass by testing nothing.
    #[test]
    fn preview_of_any_parsed_string_never_forbidden() {
        let accepted = std::cell::Cell::new(0_usize);
        proptest!(|(
            scheme in prop_oneof![
                Just("https://"),
                Just("http://"),
                Just("mailto:"),
                Just("file://"),
            ],
            tail in ".*",
            max_chars in 0_usize..256,
        )| {
            let uri = format!("{scheme}{tail}");
            if let Ok(target) = ActivationTarget::parse(&uri) {
                accepted.set(accepted.get() + 1);
                let preview = target.preview(max_chars);
                prop_assert!(!preview.chars().any(|c| c.is_control() || is_bidi_control(c)));
            }
        });
        assert!(
            accepted.get() > 0,
            "no generated case reached `preview`; the sweep asserted nothing"
        );
    }
}

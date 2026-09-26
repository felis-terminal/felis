//! Diagnostics from one config load. Validators collect these instead of
//! logging through `tracing`: `felis config check` renders the same set and
//! turns it into an exit code, which a direct log call would give it nothing
//! to render.

use tracing::{error, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// The document is unusable; the caller falls back to defaults or
    /// keeps its previous state.
    Error,
    /// Something was ignored, dropped, or clamped; the rest applies.
    Warning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticKind {
    Io,
    Parse,
    /// A key the shared schema does not define.
    UnknownKey,
    /// A known key whose value was malformed, out of range, or degraded
    /// (dropped, clamped, or the wrong shape).
    Value,
    MissingFile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub kind: DiagnosticKind,
    /// Dotted key path as written in the document (`"client.felis.font.size_px"`).
    pub key: Option<String>,
    /// Description without the key path; `Display` prepends it.
    pub message: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ConfigDiagnostics {
    entries: Vec<Diagnostic>,
}

impl ConfigDiagnostics {
    pub fn push(&mut self, diagnostic: Diagnostic) {
        self.entries.push(diagnostic);
    }

    pub(crate) fn error(
        &mut self,
        kind: DiagnosticKind,
        key: Option<String>,
        message: impl Into<String>,
    ) {
        self.push(Diagnostic {
            severity: Severity::Error,
            kind,
            key,
            message: message.into(),
        });
    }

    pub(crate) fn warning(
        &mut self,
        kind: DiagnosticKind,
        key: Option<String>,
        message: impl Into<String>,
    ) {
        self.push(Diagnostic {
            severity: Severity::Warning,
            kind,
            key,
            message: message.into(),
        });
    }

    pub fn iter(&self) -> impl Iterator<Item = &Diagnostic> {
        self.entries.iter()
    }

    pub fn errors(&self) -> impl Iterator<Item = &Diagnostic> {
        self.entries
            .iter()
            .filter(|d| d.severity == Severity::Error)
    }

    pub fn warnings(&self) -> impl Iterator<Item = &Diagnostic> {
        self.entries
            .iter()
            .filter(|d| d.severity == Severity::Warning)
    }

    /// The `felis config check` exit-`1` condition.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.entries.iter().any(|d| d.severity == Severity::Error)
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn unknown_keys(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|d| d.kind == DiagnosticKind::UnknownKey)
            .filter_map(|d| d.key.as_deref())
            .collect()
    }

    /// The single `tracing` site for config validation.
    pub fn log(&self) {
        for diagnostic in &self.entries {
            let key = diagnostic.key.as_deref().unwrap_or("-");
            match diagnostic.severity {
                Severity::Error => error!(key, message = %diagnostic.message, "config error"),
                Severity::Warning => warn!(key, message = %diagnostic.message, "config warning"),
            }
        }
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.key {
            Some(key) => write!(f, "{key}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_and_warnings_are_reported_separately() {
        let mut diagnostics = ConfigDiagnostics::default();
        diagnostics.warning(
            DiagnosticKind::UnknownKey,
            Some("window.prefix".to_owned()),
            "unknown key",
        );
        diagnostics.error(DiagnosticKind::Parse, None, "not TOML");

        assert!(diagnostics.has_errors());
        assert_eq!(diagnostics.len(), 2);
        assert_eq!(diagnostics.errors().count(), 1);
        assert_eq!(diagnostics.warnings().count(), 1);
        assert_eq!(diagnostics.unknown_keys(), ["window.prefix"]);
    }

    #[test]
    fn a_warning_only_set_is_not_an_error() {
        let mut diagnostics = ConfigDiagnostics::default();
        diagnostics.warning(
            DiagnosticKind::Value,
            Some("font.size_px".to_owned()),
            "clamped",
        );
        assert!(!diagnostics.has_errors());
        assert!(!diagnostics.is_empty());
    }

    #[test]
    fn a_keyed_diagnostic_displays_its_path() {
        let diagnostic = Diagnostic {
            severity: Severity::Warning,
            kind: DiagnosticKind::MissingFile,
            key: Some("shader.post.file".to_owned()),
            message: "no such file".to_owned(),
        };
        assert_eq!(diagnostic.to_string(), "shader.post.file: no such file");
    }
}

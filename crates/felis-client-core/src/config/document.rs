//! Reading `config.toml`, folding in one client's `[client.<id>]` overlay,
//! and resolving it to an [`EffectiveConfig`] plus [`ConfigDiagnostics`].

use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::Deserialize as _;
use tracing::warn;

use super::validate;
use super::{ConfigDiagnostics, DiagnosticKind, EffectiveConfig};

/// Neither an unknown key nor a missing file is a load failure: the
/// former warns and is skipped, the latter means defaults for
/// [`ConfigSource::Default`]. A file named by `--config` that is
/// missing is [`LoadError::Invalid`] instead.
#[derive(Debug)]
pub enum LoadError {
    /// Filesystem read failed, other than `NotFound`.
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    /// At least one error diagnostic; the complete set, warnings
    /// included, rides along.
    Invalid {
        path: Option<PathBuf>,
        diagnostics: ConfigDiagnostics,
    },
}

impl LoadError {
    #[must_use]
    pub fn diagnostics(&self) -> ConfigDiagnostics {
        match self {
            Self::Read { path, source } => {
                let mut diagnostics = ConfigDiagnostics::default();
                diagnostics.error(
                    DiagnosticKind::Io,
                    None,
                    format!("cannot read {}: {source}", path.display()),
                );
                diagnostics
            }
            Self::Invalid { diagnostics, .. } => diagnostics.clone(),
        }
    }
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(f, "config read failed ({}): {source}", path.display())
            }
            Self::Invalid { path, diagnostics } => {
                let where_ = path.as_ref().map_or_else(
                    || "config".to_owned(),
                    |path| format!("config {}", path.display()),
                );
                match diagnostics.errors().next() {
                    Some(first) => write!(f, "{where_} is invalid: {first}"),
                    None => write!(f, "{where_} is invalid"),
                }
            }
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Invalid { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigDocument {
    table: toml::Table,
    source: Option<PathBuf>,
}

impl ConfigDocument {
    /// `source` is not read; it only supplies the directory that
    /// path-valued keys resolve against.
    pub fn parse(text: &str, source: Option<PathBuf>) -> Result<Self, LoadError> {
        match text.parse::<toml::Table>() {
            Ok(table) => Ok(Self { table, source }),
            Err(err) => {
                let mut diagnostics = ConfigDiagnostics::default();
                diagnostics.error(DiagnosticKind::Parse, None, err.to_string());
                Err(LoadError::Invalid {
                    path: source,
                    diagnostics,
                })
            }
        }
    }

    /// `Ok(None)` is a missing file.
    pub fn read(path: &Path) -> Result<Option<Self>, LoadError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(LoadError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        Self::parse(&text, Some(path.to_path_buf())).map(Some)
    }

    #[must_use]
    pub fn source_path(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    /// The directory path-valued keys resolve against.
    #[must_use]
    pub fn base_dir(&self) -> Option<&Path> {
        self.source.as_deref().and_then(Path::parent)
    }

    #[must_use]
    pub const fn as_table(&self) -> &toml::Table {
        &self.table
    }

    /// Resolves the document for `client_id`, applying the client's overlay.
    ///
    /// Unknown keys warn rather than reject; other clients' sections are never
    /// validated (docs/explanation/architecture/control-surfaces.md).
    #[must_use]
    pub fn resolve(&self, client_id: &str) -> (EffectiveConfig, ConfigDiagnostics) {
        let mut diagnostics = ConfigDiagnostics::default();
        let mut doc = self.table.clone();

        // The `client` field consumes the whole `client` table, which is what
        // keeps other clients' overlays warning-free. A type error here also
        // fails the merged parse below (an overlay can replace the offending
        // value, never un-write it), so that single site reports it.
        drop(serde_ignored::deserialize::<_, _, EffectiveConfig>(
            doc.clone(),
            |path| {
                diagnostics.warning(
                    DiagnosticKind::UnknownKey,
                    Some(path.to_string()),
                    "unknown key ignored (typo, or written for another client?)",
                );
            },
        ));

        let overlay = take_overlay(&doc, client_id, &mut diagnostics);
        if let Some(overlay) = &overlay {
            drop(serde_ignored::deserialize::<_, _, EffectiveConfig>(
                overlay.clone(),
                |path| {
                    diagnostics.warning(
                        DiagnosticKind::UnknownKey,
                        Some(format!("client.{client_id}.{path}")),
                        "unknown key ignored (typo, or written for another client?)",
                    );
                },
            ));
            deep_merge(&mut doc, overlay.clone());
        }

        // Validation still runs on a type error; returning here would hand
        // `felis config check` one problem per run.
        let mut config = match EffectiveConfig::deserialize(doc.clone()) {
            Ok(config) => config,
            Err(err) => {
                diagnostics.error(DiagnosticKind::Parse, None, err.to_string());
                EffectiveConfig::default()
            }
        };
        config.source_dir = self.base_dir().map(Path::to_path_buf);
        let origin = validate::KeyOrigin::new(client_id, overlay.as_ref());
        validate::run(&config, &doc, &origin, &mut diagnostics);
        (config, diagnostics)
    }
}

/// Which document the config front doors read: platform discovery, or
/// the explicit path from `felis --config PATH`.
///
/// A missing default file uses defaults; a missing explicitly-named file is an
/// error (docs/explanation/architecture/control-surfaces.md).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ConfigSource {
    #[default]
    Default,
    /// Always absolute: `felis` resolves a relative path against its own
    /// working directory before anything else sees it.
    Explicit(PathBuf),
}

impl ConfigSource {
    /// `None` only when platform discovery finds no home directory.
    #[must_use]
    pub fn path(&self) -> Option<PathBuf> {
        match self {
            Self::Default => config_path(),
            Self::Explicit(path) => Some(path.clone()),
        }
    }

    const fn is_explicit(&self) -> bool {
        matches!(self, Self::Explicit(_))
    }

    /// Returns `Err` if an explicitly selected config file cannot be read.
    ///
    /// Reads directly rather than testing `exists()` to surface real I/O
    /// diagnostics and avoid TOCTOU races against defaults.
    pub fn require_selection(&self) -> Result<(), LoadError> {
        let Self::Explicit(path) = self else {
            return Ok(());
        };
        match ConfigDocument::read(path) {
            // A document that parses badly still selects that document;
            // only its content is the reader's problem.
            Ok(Some(_)) | Err(LoadError::Invalid { .. }) => Ok(()),
            Ok(None) => Err(LoadError::Invalid {
                path: Some(path.clone()),
                diagnostics: missing_selection_diagnostics(path),
            }),
            Err(err) => Err(err),
        }
    }
}

impl EffectiveConfig {
    /// A missing, unreadable, or invalid document yields
    /// [`EffectiveConfig::default`]. Reload paths use
    /// [`EffectiveConfig::try_load_from_source`] instead so a typo'd save
    /// keeps the live state rather than resetting it to defaults.
    #[must_use]
    pub fn load_from_source(source: &ConfigSource, client_id: &str) -> Self {
        Self::try_load_from_source(source, client_id).unwrap_or_else(|_| {
            warn!("config unusable; using built-in defaults");
            Self::default()
        })
    }

    /// A missing file is `Ok(Self::default())` under
    /// [`ConfigSource::Default`] and `Err` under an explicit selection; a
    /// read failure or an error diagnostic is always `Err`.
    pub fn try_load_from_source(source: &ConfigSource, client_id: &str) -> Result<Self, LoadError> {
        let Some(path) = source.path() else {
            return Ok(Self::default());
        };
        Self::try_load_selected(&path, client_id, source.is_explicit())
    }

    /// [`Self::load_from_source`] against platform discovery.
    #[must_use]
    pub fn load(client_id: &str) -> Self {
        Self::load_from_source(&ConfigSource::Default, client_id)
    }

    /// [`Self::try_load_from_source`] against platform discovery.
    pub fn try_load(client_id: &str) -> Result<Self, LoadError> {
        Self::try_load_from_source(&ConfigSource::Default, client_id)
    }

    /// The one place config diagnostics reach the log; the caller adds at
    /// most one line saying what it did about them.
    pub fn try_load_from(path: &Path, client_id: &str) -> Result<Self, LoadError> {
        Self::try_load_selected(path, client_id, false)
    }

    /// `selected` says how an absent file reads: the first-run case
    /// under discovery, a typo under a path the user typed.
    fn try_load_selected(path: &Path, client_id: &str, selected: bool) -> Result<Self, LoadError> {
        let document = match ConfigDocument::read(path) {
            Ok(Some(document)) => document,
            Ok(None) if selected => {
                let diagnostics = missing_selection_diagnostics(path);
                diagnostics.log();
                return Err(LoadError::Invalid {
                    path: Some(path.to_path_buf()),
                    diagnostics,
                });
            }
            Ok(None) => return Ok(Self::default()),
            Err(err) => {
                err.diagnostics().log();
                return Err(err);
            }
        };
        let (config, diagnostics) = document.resolve(client_id);
        diagnostics.log();
        if diagnostics.has_errors() {
            return Err(LoadError::Invalid {
                path: Some(path.to_path_buf()),
                diagnostics,
            });
        }
        Ok(config)
    }

    /// Folds every failure, a read error included, into the diagnostics
    /// and logs none of it: the entry point for `felis config check` /
    /// `show-effective`, which render the set themselves.
    #[must_use]
    pub fn diagnose(path: &Path, client_id: &str) -> (Self, ConfigDiagnostics) {
        match ConfigDocument::read(path) {
            Ok(Some(document)) => document.resolve(client_id),
            Ok(None) => (Self::default(), ConfigDiagnostics::default()),
            Err(err) => (Self::default(), err.diagnostics()),
        }
    }

    /// [`Self::diagnose`] against a selection, so an explicitly named
    /// file that is not there is an error rather than silence.
    #[must_use]
    pub fn diagnose_source(source: &ConfigSource, client_id: &str) -> (Self, ConfigDiagnostics) {
        let Some(path) = source.path() else {
            return (Self::default(), ConfigDiagnostics::default());
        };
        match ConfigDocument::read(&path) {
            Ok(Some(document)) => document.resolve(client_id),
            Ok(None) if source.is_explicit() => {
                (Self::default(), missing_selection_diagnostics(&path))
            }
            Ok(None) => (Self::default(), ConfigDiagnostics::default()),
            Err(err) => (Self::default(), err.diagnostics()),
        }
    }
}

fn missing_selection_diagnostics(path: &Path) -> ConfigDiagnostics {
    let mut diagnostics = ConfigDiagnostics::default();
    diagnostics.error(
        DiagnosticKind::MissingFile,
        None,
        format!("no config file at {}", path.display()),
    );
    diagnostics
}

/// Leaves the `client` table itself in place: the `client` field carries
/// every overlay verbatim.
pub(super) fn take_overlay(
    doc: &toml::Table,
    client_id: &str,
    diagnostics: &mut ConfigDiagnostics,
) -> Option<toml::Table> {
    let overlay = doc.get("client")?.get(client_id)?;
    let Some(overlay) = overlay.as_table() else {
        diagnostics.warning(
            DiagnosticKind::Value,
            Some(format!("client.{client_id}")),
            "client overlay is not a table; ignoring",
        );
        return None;
    };
    let mut overlay = overlay.clone();
    if overlay.remove("client").is_some() {
        diagnostics.warning(
            DiagnosticKind::Value,
            Some(format!("client.{client_id}.client")),
            "an overlay may not nest overlays; ignoring",
        );
    }
    Some(overlay)
}

/// Arrays replace rather than merge element-wise: an element-wise merge
/// cannot express removal (a shorter `font.fallback` chain in the overlay
/// must win, not be padded back to the base's length).
pub(super) fn deep_merge(base: &mut toml::Table, overlay: toml::Table) {
    for (key, value) in overlay {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(base_sub)), toml::Value::Table(overlay_sub)) => {
                deep_merge(base_sub, overlay_sub);
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

/// `$XDG_CONFIG_HOME/felis/config.toml` on Linux, the platform equivalent
/// elsewhere; `None` when no home directory is discoverable (CI sandboxes).
#[must_use]
pub fn config_path() -> Option<PathBuf> {
    Some(config_dir()?.join("config.toml"))
}

fn config_dir() -> Option<PathBuf> {
    // Empty qualifier and organization, matching every other felis
    // `ProjectDirs` call: macOS joins them into a bundle id, so
    // `("dev", "felis", "felis")` would move this directory to
    // `Library/Application Support/dev.felis.felis`.
    let dirs = ProjectDirs::from("", "", "felis")?;
    Some(dirs.config_dir().to_path_buf())
}

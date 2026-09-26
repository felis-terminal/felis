//! Validation pass over a parsed and merged config document. A bad value
//! is dropped or clamped, never fatal.

use std::path::Path;

use serde::Deserialize as _;
use serde::de::DeserializeOwned;

use super::{
    Backdrop, ConfigDiagnostics, CursorBlinkMode, DiagnosticKind, EffectiveConfig, FontSizeIssue,
    IndexedSlotError, Osc52Policy, PostShader, RangeIssue, ShaderAnimation, fallback_entry,
    indexed_slot, unknown_post_shader_token,
};

/// Which half of the document a merged value came from. The merged table
/// has forgotten whether a value was written under `[client.<id>]`, and
/// reporting the merged spelling would send the user to a line their file
/// does not contain.
pub(super) struct KeyOrigin<'a> {
    client_id: &'a str,
    overlay: Option<&'a toml::Table>,
}

impl<'a> KeyOrigin<'a> {
    pub(super) const fn new(client_id: &'a str, overlay: Option<&'a toml::Table>) -> Self {
        Self { client_id, overlay }
    }

    /// `path` is the key as lookup segments; `written` is how the
    /// diagnostic spells it (an array index, a quoted chord).
    fn attribute(&self, path: &[&str], written: &str) -> String {
        match self.overlay {
            Some(overlay) if lookup(overlay, path).is_some() => {
                format!("client.{}.{written}", self.client_id)
            }
            _ => written.to_owned(),
        }
    }
}

fn lookup<'t>(table: &'t toml::Table, path: &[&str]) -> Option<&'t toml::Value> {
    let (first, rest) = path.split_first()?;
    let mut value = table.get(*first)?;
    for segment in rest {
        value = value.as_table()?.get(*segment)?;
    }
    Some(value)
}

pub(super) fn run(
    config: &EffectiveConfig,
    document: &toml::Table,
    origin: &KeyOrigin<'_>,
    diagnostics: &mut ConfigDiagnostics,
) {
    font_size(config, origin, diagnostics);
    window_opacity(config, origin, diagnostics);
    mouse_scroll_multiplier(config, origin, diagnostics);
    fallback_chain(document, origin, diagnostics);
    palette(config, origin, diagnostics);
    keymap(config, origin, diagnostics);
    shader(config, origin, diagnostics);
    closed_enums(document, origin, diagnostics);
}

/// Re-validates from the merged document: an unknown token is degraded
/// to the field's default during deserialization and leaves nothing in
/// [`EffectiveConfig`] to distinguish it from the default written out.
fn closed_enums(
    document: &toml::Table,
    origin: &KeyOrigin<'_>,
    diagnostics: &mut ConfigDiagnostics,
) {
    enum_token::<CursorBlinkMode>(
        document,
        &["cursor", "blink"],
        "program",
        origin,
        diagnostics,
    );
    enum_token::<ShaderAnimation>(
        document,
        &["shader", "animate"],
        "never",
        origin,
        diagnostics,
    );
    enum_token::<Backdrop>(
        document,
        &["window", "backdrop"],
        "none",
        origin,
        diagnostics,
    );
    enum_token::<Osc52Policy>(
        document,
        &["clipboard", "osc_52"],
        "mirror",
        origin,
        diagnostics,
    );
    post_shader_token(document, origin, diagnostics);
}

fn enum_token<T: DeserializeOwned>(
    document: &toml::Table,
    path: &[&str],
    fallback: &str,
    origin: &KeyOrigin<'_>,
    diagnostics: &mut ConfigDiagnostics,
) {
    let Some(value) = lookup(document, path) else {
        return;
    };
    // A non-string failed the document parse as a shape error, which
    // already reported itself.
    let Some(token) = value.as_str() else {
        return;
    };
    if T::deserialize(value.clone()).is_ok() {
        return;
    }
    diagnostics.warning(
        DiagnosticKind::Value,
        Some(origin.attribute(path, &path.join("."))),
        format!("unknown value `{token}`; using the default `{fallback}`"),
    );
}

fn post_shader_token(
    document: &toml::Table,
    origin: &KeyOrigin<'_>,
    diagnostics: &mut ConfigDiagnostics,
) {
    let path = ["shader", "post"];
    let Some(value) = lookup(document, &path) else {
        return;
    };
    if PostShader::deserialize(value.clone()).is_ok() || !unknown_post_shader_token(value) {
        return;
    }
    let Some((tag, payload)) = value.as_table().and_then(|table| table.iter().next()) else {
        return;
    };
    let (written, message) = if tag == "builtin" {
        let token = payload.as_str().unwrap_or_default();
        (
            "shader.post.builtin",
            format!("unknown builtin shader `{token}`; no post-process pass"),
        )
    } else {
        (
            "shader.post",
            format!("unknown shader selector `{tag}`; no post-process pass"),
        )
    };
    diagnostics.warning(
        DiagnosticKind::Value,
        Some(origin.attribute(&path, written)),
        message,
    );
}

fn font_size(
    config: &EffectiveConfig,
    origin: &KeyOrigin<'_>,
    diagnostics: &mut ConfigDiagnostics,
) {
    let key = || Some(origin.attribute(&["font", "size_px"], "font.size_px"));
    match config.font.font_size_issue() {
        None => {}
        Some(FontSizeIssue::NotPositiveFinite(raw)) => diagnostics.warning(
            DiagnosticKind::Value,
            key(),
            format!("{raw} is not positive-finite; using the default size"),
        ),
        Some(FontSizeIssue::Clamped { raw, clamped }) => diagnostics.warning(
            DiagnosticKind::Value,
            key(),
            format!(
                "{raw} is outside [{}, {}] logical px; clamped to {clamped}",
                super::MIN_FONT_SIZE_LOGICAL_PX,
                super::MAX_FONT_SIZE_LOGICAL_PX,
            ),
        ),
    }
}

fn window_opacity(
    config: &EffectiveConfig,
    origin: &KeyOrigin<'_>,
    diagnostics: &mut ConfigDiagnostics,
) {
    let Some(issue) = config.window.opacity_issue() else {
        return;
    };
    diagnostics.warning(
        DiagnosticKind::Value,
        Some(origin.attribute(&["window", "opacity"], "window.opacity")),
        issue.message("[0.0, 1.0]"),
    );
}

fn mouse_scroll_multiplier(
    config: &EffectiveConfig,
    origin: &KeyOrigin<'_>,
    diagnostics: &mut ConfigDiagnostics,
) {
    let Some(issue) = config.mouse.scroll_multiplier_issue() else {
        return;
    };
    diagnostics.warning(
        DiagnosticKind::Value,
        Some(origin.attribute(&["mouse", "scroll_multiplier"], "mouse.scroll_multiplier")),
        issue.message("[0.1, 100.0]"),
    );
}

impl RangeIssue {
    fn message(&self, band: &str) -> String {
        match self {
            Self::NotFinite { fallback } => format!("not a finite number; using {fallback}"),
            Self::Clamped { raw, clamped } => {
                format!("{raw} is outside {band}; clamped to {clamped}")
            }
        }
    }
}

/// Re-validates from the merged document: a malformed entry is dropped
/// during deserialization and leaves nothing in [`EffectiveConfig`].
fn fallback_chain(
    document: &toml::Table,
    origin: &KeyOrigin<'_>,
    diagnostics: &mut ConfigDiagnostics,
) {
    let Some(entries) = document
        .get("font")
        .and_then(|font| font.get("fallback"))
        .and_then(toml::Value::as_array)
    else {
        return;
    };
    for (index, value) in entries.iter().enumerate() {
        if let Err(err) = fallback_entry(value.clone()) {
            diagnostics.warning(
                DiagnosticKind::Value,
                Some(origin.attribute(&["font", "fallback"], &format!("font.fallback[{index}]"))),
                format!("malformed entry, skipped: {err}"),
            );
        }
    }
}

fn palette(config: &EffectiveConfig, origin: &KeyOrigin<'_>, diagnostics: &mut ConfigDiagnostics) {
    for key in config.theme.palette.indexed.keys() {
        let message = match indexed_slot(key) {
            Ok(_) => continue,
            Err(IndexedSlotError::NamedSlot) => {
                "a base ANSI slot; set it by name (0-15 are named fields), ignoring".to_owned()
            }
            Err(IndexedSlotError::NotAnIndex) => "not an index in 16..=255; ignoring".to_owned(),
        };
        diagnostics.warning(
            DiagnosticKind::Value,
            Some(origin.attribute(
                &["theme", "palette", "indexed", key],
                &format!("theme.palette.indexed.{key}"),
            )),
            message,
        );
    }
}

fn keymap(config: &EffectiveConfig, origin: &KeyOrigin<'_>, diagnostics: &mut ConfigDiagnostics) {
    drop(
        config
            .keymap
            .compile_reporting(config.source_dir.as_deref(), &mut |chord, reason| {
                diagnostics.warning(
                    DiagnosticKind::Value,
                    Some(origin.attribute(&["keymap", chord], &format!("keymap.\"{chord}\""))),
                    reason,
                );
            }),
    );
}

/// A missing shader file is a warning, not an error: the document may be
/// shared with a machine where the file exists.
fn shader(config: &EffectiveConfig, origin: &KeyOrigin<'_>, diagnostics: &mut ConfigDiagnostics) {
    let Some(PostShader::File(raw)) = &config.shader.post else {
        return;
    };
    let resolved = super::resolve_path(Path::new(raw), config.source_dir.as_deref());
    if !resolved.is_file() {
        diagnostics.warning(
            DiagnosticKind::MissingFile,
            Some(origin.attribute(&["shader", "post"], "shader.post.file")),
            format!(
                "no such file: {} (resolved from `{raw}`); no post-process pass",
                resolved.display()
            ),
        );
    }
}

//! User-facing configuration shared by felis clients: [`ConfigDocument`],
//! [`EffectiveConfig`], and [`ConfigDiagnostics`].
//!
//! Lenient parsing is the contract: unknown keys are reported, never rejected
//! (docs/explanation/architecture/control-surfaces.md).

mod diagnostics;
mod document;
mod validate;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use diagnostics::{ConfigDiagnostics, Diagnostic, DiagnosticKind, Severity};
pub use document::{ConfigDocument, ConfigSource, LoadError, config_path};

use crate::keymap::{BindingValue, Chord, PipeSink};

/// The GUI client's overlay id (`[client.felis]`). A constant rather than
/// the binary's name, so a rename cannot orphan everyone's overlay; it
/// lives here because `felis-cli` resolves the same overlay and cannot
/// depend on `felis-client`.
pub const GUI_CLIENT_ID: &str = "felis";

/// One client's view of the felis `config.toml`: defaults filled in and
/// its `[client.<id>]` overlay folded in. Every key is optional:
/// omitted keys keep built-in defaults, unknown keys are reported and ignored.
// Deserializing directly skips overlay merge and validation; use `ConfigDocument::resolve`.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(title = "felis configuration"))]
#[cfg_attr(
    feature = "schema",
    schemars(extend("$comment" = "Deliberately non-strict at the section level: neither this object nor any config section closes with `additionalProperties: false`, because the document is shared with other clients and an unrecognized section key warns at load time rather than failing it. Nested typed values, such as a `keymap` binding's fields, are still validated by shape and do close. For the same reason `client` validates only the `felis` section; every other client's section is inert and unvalidated. An `enum` here lists the tokens this build understands, so an editor flags a newer file's token while the runtime keeps the document loading, degrading a field with a default or dropping the one keymap binding. The runtime parser, not this schema, is the authority."))
)]
#[serde(default)]
pub struct EffectiveConfig {
    /// Font selection (family, size).
    pub font: FontConfig,
    /// Default foreground / background and the 16-color palette.
    pub theme: ThemeConfig,
    /// Clipboard policy (OS-clipboard backend on/off, OSC 52 to
    /// system gate).
    pub clipboard: ClipboardConfig,
    /// Window-level settings (title prefix, …).
    pub window: WindowConfig,
    /// User overrides to the default keymap. Each key is a chord
    /// string (`"ctrl+shift+f"`); each value is a typed binding
    /// value. See docs/reference/keybindings.md for the grammar and merge semantics.
    pub keymap: KeymapConfig,
    /// Cursor color and blink behavior.
    pub cursor: CursorConfig,
    /// Mouse / wheel behavior (scroll speed).
    pub mouse: MouseConfig,
    /// Post-process shader selection.
    pub shader: ShaderConfig,
    /// Per-client overlays: `[client.<name>]` is deep-merged over the top level.
    /// Sections for other clients are carried untouched (never validated, never
    /// warned about) so one file can serve clients whose schemas differ.
    #[cfg_attr(feature = "schema", schemars(schema_with = "client_overlay_schema"))]
    pub client: BTreeMap<String, toml::Value>,
    /// The directory file-valued keys resolve against; `None` for a config
    /// parsed from text, whose relative paths then resolve against the
    /// working directory.
    // Public because a private field makes `..Default::default()` refuse
    // to build the struct outside this crate (E0451).
    #[serde(skip)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    pub source_dir: Option<PathBuf>,
}

// Not derived: `source_dir` is provenance, and comparing it would make a
// config read from a file unequal to the same config parsed from text.
impl PartialEq for EffectiveConfig {
    fn eq(&self, other: &Self) -> bool {
        let Self {
            font,
            theme,
            clipboard,
            window,
            keymap,
            cursor,
            mouse,
            shader,
            client,
            source_dir: _,
        } = self;
        *font == other.font
            && *theme == other.theme
            && *clipboard == other.clipboard
            && *window == other.window
            && *keymap == other.keymap
            && *cursor == other.cursor
            && *mouse == other.mouse
            && *shader == other.shader
            && *client == other.client
    }
}

impl EffectiveConfig {
    #[must_use]
    pub fn post_shader_choice(&self) -> PostShaderChoice {
        self.shader.post_choice(self.source_dir.as_deref())
    }
}

// `BTreeMap<String, EffectiveConfig>` is the rejected shape: it would make
// an editor validate every foreign section against this vocabulary, which
// the runtime never does.
#[cfg(feature = "schema")]
fn client_overlay_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let own = generator.subschema_for::<EffectiveConfig>();
    schemars::json_schema!({
        "type": "object",
        "properties": { GUI_CLIENT_ID: own },
        "additionalProperties": true,
    })
}

/// `[cursor]` section: everything the user controls about the cursor:
/// its color and its blink animation. The *shape* is the running
/// program's (`DECSCUSR`), and a runtime `OSC 12` overrides the
/// configured color for as long as the session sets it.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct CursorConfig {
    /// `#rrggbb` for the cursor block. Missing ⇒ reverse-video cursor
    /// (theme-agnostic, always visible).
    pub color: Option<String>,
    /// When the cursor blinks. Default `program` blinks only when the
    /// running program requests it via `DECSCUSR` (xterm / kitty
    /// behavior; the default cursor requests blink). `never` is the
    /// reduced-motion / accessibility lever; `always` forces blink even
    /// for a program-requested steady cursor.
    #[serde(deserialize_with = "deserialize_lenient_enum")]
    pub blink: CursorBlinkMode,
    /// Half-period of the blink, in milliseconds (how long the cursor
    /// stays shown, then hidden). The default 530 ms is xterm's
    /// historical value. Very small values are raised to a safe floor.
    pub blink_interval_ms: u64,
}

impl Default for CursorConfig {
    fn default() -> Self {
        Self {
            color: None,
            blink: CursorBlinkMode::default(),
            blink_interval_ms: 530,
        }
    }
}

/// Cursor blink policy (`cursor.blink`).
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CursorBlinkMode {
    /// Blink only when the program asks (`DECSCUSR` odd-`Ps`). Default.
    #[default]
    Program,
    /// Never blink, whatever the program requests.
    Never,
    /// Always blink, even for a program-requested steady cursor.
    Always,
}

/// `[shader]` section: an optional WGSL post-process pass applied to
/// the rendered frame. Clients that draw no pixels ignore this
/// section.
#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct ShaderConfig {
    /// Post-process shader, absent by default (no extra pass).
    /// `{ builtin = "trail" }` selects a shader felis ships;
    /// `{ file = "…" }` a WGSL file of your own. felis never scans a
    /// directory: what runs is what the config named.
    #[serde(deserialize_with = "deserialize_lenient_post_shader")]
    pub post: Option<PostShader>,
    /// Whether the shader gets frames of its own. Default `never`:
    /// felis draws when something changed, and the shader sees those
    /// frames. `focused` adds a continuous redraw clock for effects
    /// that animate off `time_s` and have no other way to advance.
    #[serde(deserialize_with = "deserialize_lenient_enum")]
    pub animate: ShaderAnimation,
}

/// Redraw policy for a loaded post-process shader (`shader.animate`).
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ShaderAnimation {
    /// No dedicated clock: frames happen for the usual reasons (input,
    /// program output, cursor moves), and the shader animates only
    /// through the uniforms felis eases for it. Default, and the only
    /// value that keeps idle CPU at zero.
    #[default]
    Never,
    /// Redraw continuously while the window holds focus, for a shader
    /// that animates purely off `time_s` and would otherwise stand
    /// still. There is deliberately no variant that animates unfocused
    /// windows: a background window must never burn CPU.
    Focused,
}

/// What `shader.post` names: a shader felis ships, or a file.
// Not one string carrying both meanings: classifying a bare name against
// a path would be felis sniffing a user's value (principle 4).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PostShader {
    /// A shader felis carries in the binary.
    Builtin(BuiltinShader),
    /// A WGSL file at this path. A leading `~/` expands and an absolute
    /// path is taken as written; a relative path resolves against the
    /// directory holding the `config.toml` that named it.
    File(String),
}

/// The shaders felis ships (`shader.post = { builtin = "…" }`). Each
/// is an ordinary shader on the contract in
/// docs/reference/shaders.md; there is no privileged path.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum BuiltinShader {
    /// The cursor trail.
    Trail,
}

/// [`ShaderConfig::post`] with its file path resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostShaderChoice {
    None,
    /// The caller supplies the source.
    Builtin(BuiltinShader),
    File(PathBuf),
}

impl ShaderConfig {
    #[must_use]
    pub fn post_choice(&self, base_dir: Option<&Path>) -> PostShaderChoice {
        match &self.post {
            None => PostShaderChoice::None,
            Some(PostShader::Builtin(builtin)) => PostShaderChoice::Builtin(*builtin),
            Some(PostShader::File(path)) => {
                PostShaderChoice::File(resolve_path(Path::new(path), base_dir))
            }
        }
    }
}

/// Deserializes a closed config enum leniently: an unknown *string*
/// token degrades this one field to its default, while any other shape
/// stays an error that fails the document. `validate::closed_enums`
/// re-reads the merged document to warn with the field's full path.
fn deserialize_lenient_enum<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let value = toml::Value::deserialize(deserializer)?;
    let is_token = value.is_str();
    T::deserialize(value).or_else(|err| {
        if is_token {
            Ok(T::default())
        } else {
            Err(serde::de::Error::custom(err))
        }
    })
}

/// [`deserialize_lenient_enum`] for `shader.post`, whose unknown token
/// can be the outer selector or the builtin's name, and whose
/// documented default is no post-process pass.
fn deserialize_lenient_post_shader<'de, D>(deserializer: D) -> Result<Option<PostShader>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = toml::Value::deserialize(deserializer)?;
    match PostShader::deserialize(value.clone()) {
        Ok(shader) => Ok(Some(shader)),
        Err(_) if unknown_post_shader_token(&value) => Ok(None),
        Err(err) => Err(serde::de::Error::custom(err)),
    }
}

/// Whether a rejected `shader.post` names something this build does not
/// know rather than being malformed: a selector felis has no variant
/// for, or `builtin` naming a shader it does not ship. A bad payload
/// under a known selector (`file = 3`) is not a token.
pub(super) fn unknown_post_shader_token(value: &toml::Value) -> bool {
    let Some(table) = value.as_table() else {
        return false;
    };
    let mut entries = table.iter();
    let (Some((tag, payload)), None) = (entries.next(), entries.next()) else {
        return false;
    };
    match tag.as_str() {
        "builtin" => payload.is_str(),
        "file" => false,
        _ => true,
    }
}

/// Relative paths resolve against the config file's directory, not the
/// working directory: a shader beside `config.toml` must not stop loading
/// because felis was launched from somewhere else.
pub(super) fn resolve_path(name: &Path, base_dir: Option<&Path>) -> PathBuf {
    // `strip_prefix` alone would also expand a bare `~`, which names a
    // file called `~` rather than the home directory; the length check
    // is what tells `~` from `~/`, whose remainder is also empty.
    if let Ok(rest) = name.strip_prefix("~")
        && name.as_os_str().len() > 1
    {
        return directories::BaseDirs::new()
            .map_or_else(|| name.to_path_buf(), |dirs| dirs.home_dir().join(rest));
    }
    match base_dir {
        Some(base) if name.is_relative() => base.join(name),
        _ => name.to_path_buf(),
    }
}

/// `[mouse]` section: wheel scroll speed.
// Linux delivers a wheel notch as exactly one line with no OS-level
// acceleration (winit reports `LineDelta(0, ±1)` on Wayland and X11),
// where macOS scales line deltas by gesture speed.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct MouseConfig {
    /// Lines scrolled per wheel notch. The default 3 matches alacritty
    /// (`scrolling.multiplier`) and kitty (`wheel_scroll_multiplier`),
    /// and applies only to notch-based wheels: touchpad smooth
    /// scrolling is already velocity-scaled by the OS. Clamped to
    /// `[0.1, 100.0]` with a warning; `nan` uses the default.
    pub scroll_multiplier: f64,
}

const DEFAULT_SCROLL_MULTIPLIER: f64 = 3.0;
const MIN_SCROLL_MULTIPLIER: f64 = 0.1;
const MAX_SCROLL_MULTIPLIER: f64 = 100.0;

impl Default for MouseConfig {
    fn default() -> Self {
        Self {
            scroll_multiplier: DEFAULT_SCROLL_MULTIPLIER,
        }
    }
}

impl MouseConfig {
    /// Floored above zero so a negative value cannot flip scroll direction.
    // `f64::clamp` passes NaN through, and a NaN multiplier zeroes every
    // wheel delta downstream.
    #[must_use]
    pub const fn clamped_scroll_multiplier(self) -> f64 {
        if self.scroll_multiplier.is_nan() {
            DEFAULT_SCROLL_MULTIPLIER
        } else {
            self.scroll_multiplier
                .clamp(MIN_SCROLL_MULTIPLIER, MAX_SCROLL_MULTIPLIER)
        }
    }

    pub(super) fn scroll_multiplier_issue(self) -> Option<RangeIssue> {
        RangeIssue::detect(
            self.scroll_multiplier,
            MIN_SCROLL_MULTIPLIER,
            MAX_SCROLL_MULTIPLIER,
            DEFAULT_SCROLL_MULTIPLIER,
        )
    }
}

/// A value the loader silently repaired, reported by `validate.rs`.
pub(super) enum RangeIssue {
    NotFinite { fallback: f64 },
    Clamped { raw: f64, clamped: f64 },
}

impl RangeIssue {
    fn detect(raw: f64, min: f64, max: f64, fallback: f64) -> Option<Self> {
        if raw.is_nan() {
            return Some(Self::NotFinite { fallback });
        }
        // A value a hair outside the band still clamps, so the test is the
        // band itself: the gap to the bound can be far below `f64::EPSILON`.
        (raw < min || raw > max).then_some(Self::Clamped {
            raw,
            clamped: raw.clamp(min, max),
        })
    }
}

/// `[keymap]` section: chord string (`"ctrl+shift+f"`) → binding
/// table. Entries are validated one by one: a malformed entry is
/// reported and skipped rather than aborting the whole block. See
/// docs/reference/keybindings.md for the chord grammar and merge
/// semantics.
#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq)]
#[cfg_attr(
    feature = "schema",
    derive(schemars::JsonSchema),
    schemars(transparent)
)]
#[serde(transparent)]
pub struct KeymapConfig {
    #[cfg_attr(feature = "schema", schemars(with = "BTreeMap<String, BindingValue>"))]
    entries: BTreeMap<String, toml::Value>,
}

impl KeymapConfig {
    /// Malformed entries are dropped silently; the loader reports them
    /// through [`ConfigDiagnostics`]. `base_dir` is the directory the
    /// bindings' file-valued fields resolve against
    /// ([`EffectiveConfig::source_dir`]).
    #[must_use]
    pub fn compile(&self, base_dir: Option<&Path>) -> Vec<(Chord, BindingValue)> {
        self.compile_reporting(base_dir, &mut |_, _| {})
    }

    pub(super) fn compile_reporting(
        &self,
        base_dir: Option<&Path>,
        on_dropped: &mut dyn FnMut(&str, String),
    ) -> Vec<(Chord, BindingValue)> {
        let mut out = Vec::with_capacity(self.entries.len());
        for (chord_str, value) in &self.entries {
            let chord: Chord = match chord_str.parse() {
                Ok(c) => c,
                Err(err) => {
                    on_dropped(
                        chord_str,
                        format!("malformed chord string, dropping: {err}"),
                    );
                    continue;
                }
            };
            let binding: BindingValue = match value.clone().try_into() {
                Ok(b) => b,
                Err(err) => {
                    on_dropped(
                        chord_str,
                        format!("malformed binding value, dropping: {err}"),
                    );
                    continue;
                }
            };
            // serde only enforces the field's presence; an empty argv
            // would swallow the chord and launch nothing.
            if matches!(&binding, BindingValue::Run { command } if command.is_empty()) {
                on_dropped(
                    chord_str,
                    "`run` with an empty command, dropping".to_owned(),
                );
                continue;
            }
            // An unrecognized backslash sequence is a config error
            // (docs/explanation/input.md "Action mapping"), never sent
            // literally by the dispatcher.
            if let BindingValue::SendString { text, escapes } = &binding
                && let Err(err) = escapes.decode(text)
            {
                on_dropped(
                    chord_str,
                    format!("invalid send_string escape, dropping: {err}"),
                );
                continue;
            }
            out.push((chord, resolve_binding_paths(binding, base_dir)));
        }
        out
    }
}

/// A `pipe` file sink is a file-valued key, so it resolves here rather
/// than against whatever working directory the window was launched with.
fn resolve_binding_paths(binding: BindingValue, base_dir: Option<&Path>) -> BindingValue {
    match binding {
        BindingValue::Pipe {
            source,
            target: PipeSink::File(path),
            ansi,
        } => BindingValue::Pipe {
            source,
            target: PipeSink::File(resolve_path(&path, base_dir)),
            ansi,
        },
        other => other,
    }
}

/// Window-level settings.
///
/// Configures window decorations, background opacity, backdrops,
/// and an optional title prefix attached to the shell's dynamic title.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct WindowConfig {
    /// Optional prefix glued in front of the dynamic shell title.
    /// Format: `"<prefix> <title>"`. Empty or missing means no prefix;
    /// the title is whatever the shell sets.
    pub title_prefix: Option<String>,
    /// Whether the OS-drawn window chrome (title bar, borders, system buttons)
    /// is shown. `true` (default) keeps native decorations; `false` strips them.
    /// On macOS the window keeps its rounded corners and shadow. The terminal
    /// paints no replacement chrome.
    pub decorations: bool,
    /// Background opacity in `0.0..=1.0` (`1.0` default).
    ///
    /// Only the default background bleeds through; explicit cell backgrounds
    /// stay solid. Decided once at startup: changing between opaque and
    /// translucent requires a restart. Clamped to `0.0..=1.0`.
    pub opacity: f32,
    /// OS-native backdrop hosted behind the window (`none` by default).
    ///
    /// `blur` (macOS) requires `opacity < 1.0`. DWM backdrops (Windows) style
    /// the title bar regardless of opacity. On Linux, blur belongs to the
    /// compositor. Unsupported backdrops log a note and leave the window plain.
    #[serde(deserialize_with = "deserialize_lenient_enum")]
    pub backdrop: Backdrop,
}

/// The OS-native backdrop behind the window (`window.backdrop`).
/// `blur` hosts an `NSVisualEffectView` on macOS; the rest are the DWM
/// system backdrops (`DWMWA_SYSTEMBACKDROP_TYPE`) on Windows.
// No `auto` in the DWM set: DWM already applies it when no backdrop is
// set, which is what `none` leaves in place.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Backdrop {
    /// No backdrop: the window frame is opaque (the default).
    #[default]
    None,
    /// macOS: the `NSVisualEffectView` frosted backdrop. Needs
    /// `opacity < 1.0` to show through.
    Blur,
    /// Windows: acrylic (`DWMSBT_TRANSIENTWINDOW`), a heavier
    /// translucent blur.
    Acrylic,
    /// Windows: mica (`DWMSBT_MAINWINDOW`), the Win11 desktop-tinted
    /// material.
    Mica,
    /// Windows: tabbed (`DWMSBT_TABBEDWINDOW`), the Mica variant tuned
    /// for tabbed title bars.
    Tabbed,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            title_prefix: None,
            decorations: true,
            opacity: 1.0,
            backdrop: Backdrop::None,
        }
    }
}

impl WindowConfig {
    /// `NaN` is treated as fully opaque.
    #[must_use]
    pub const fn clamped_opacity(&self) -> f32 {
        if self.opacity.is_nan() {
            1.0
        } else {
            self.opacity.clamp(MIN_OPACITY, MAX_OPACITY)
        }
    }

    pub(super) fn opacity_issue(&self) -> Option<RangeIssue> {
        RangeIssue::detect(
            f64::from(self.opacity),
            f64::from(MIN_OPACITY),
            f64::from(MAX_OPACITY),
            f64::from(MAX_OPACITY),
        )
    }
}

const MIN_OPACITY: f32 = 0.0;
const MAX_OPACITY: f32 = 1.0;

/// Font selection.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct FontConfig {
    /// Font family name (e.g. `"JetBrains Mono"`). Omitted ⇒ the
    /// system monospace font.
    pub family: Option<String>,
    /// Font size in logical pixels: the client multiplies it by the
    /// window's display scale, so one value holds across a high-DPI and
    /// a standard-DPI monitor. Positive values clamp into `[4.0, 72.0]`
    /// (the Ctrl+Wheel zoom band) with a warning; non-positive,
    /// non-finite, and omitted values use the built-in default.
    pub size_px: Option<f32>,
    /// Explicit per-grapheme fallback chain, appended in declared order
    /// (missing families warn and are skipped). Empty (default) runs the
    /// CJK / symbol / emoji / Nerd Font auto-discovery instead, never on
    /// top. Each entry is `{ family = "...", features = [...] }`, where an
    /// omitted `features` inherits `font.features` (docs/reference/config.md).
    #[serde(deserialize_with = "deserialize_fallback_chain")]
    pub fallback: Vec<FontStyleConfig>,
    /// OpenType feature tags applied to the primary face and inherited by
    /// fallback fonts. Use a 4-char tag to enable (`"calt"`) or leading `-`
    /// to disable (`"-clig"`). Empty by default (features off).
    pub features: Vec<String>,
    /// `[font.bold]`: the face for bold cells (kitty's `bold_font`).
    /// A missing table (or a missing `family` within it) derives the
    /// bold face from `font.family` at bold weight.
    pub bold: FontStyleConfig,
    /// `[font.italic]`: the face for italic cells (kitty's
    /// `italic_font`).
    pub italic: FontStyleConfig,
    /// `[font.bold_italic]`: the face for bold+italic cells (kitty's
    /// `bold_italic_font`).
    pub bold_italic: FontStyleConfig,
}

/// One font-face table: a per-style override or fallback entry.
///
/// If a style's named family has no matching face installed, the primary face
/// is reused; felis does not synthesize fake bold or oblique
/// (docs/explanation/rendering/text-shaping.md).
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct FontStyleConfig {
    /// Family override. Omitted ⇒ derived from the base `font.family`
    /// at this style's weight/slant.
    // Omitted rather than serialized as null: a `font.fallback` entry is
    // read back through `toml::Value`, which has no null, so a `null`
    // would make `config show-effective` output un-re-readable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Feature override. Omitted ⇒ inherits the base `font.features`;
    /// an explicit `[]` opts out of every feature for this style; a
    /// non-empty list replaces the base list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub features: Option<Vec<String>>,
}

/// Per-entry lenient: a plain `Vec<FontStyleConfig>` would fail the whole
/// `[font]` parse on the first bad entry (docs/reference/config.md failure
/// matrix). Skipping is silent because a deserializer has no diagnostics
/// sink; the validation pass re-walks the raw entries and reports them.
fn deserialize_fallback_chain<'de, D>(deserializer: D) -> Result<Vec<FontStyleConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<toml::Value>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .filter_map(|value| fallback_entry(value).ok())
        .collect())
}

/// Unknown keys are rejected here because the loader's `serde_ignored`
/// sweep cannot see inside this value-level parse: `{ familly = "…" }`
/// would otherwise become an all-default entry and vanish silently.
/// `family` is required, unlike the per-style overrides sharing
/// [`FontStyleConfig`]: an entry with no family names no font.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FallbackEntry {
    family: String,
    #[serde(default)]
    features: Option<Vec<String>>,
}

pub(super) fn fallback_entry(value: toml::Value) -> Result<FontStyleConfig, toml::de::Error> {
    let FallbackEntry { family, features } = FallbackEntry::deserialize(value)?;
    Ok(FontStyleConfig {
        family: Some(family),
        features,
    })
}

/// Default foreground / background colors.
///
/// Every hex string field here is `#rrggbb`. A malformed value logs a
/// warning and falls back to its default; the rest of the theme is unaffected.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct ThemeConfig {
    /// `#rrggbb` for the default foreground.
    pub foreground: Option<String>,
    /// `#rrggbb` for the default background.
    pub background: Option<String>,
    /// Per-slot overrides for the SGR 16-color palette.
    pub palette: PaletteConfig,
}

/// Per-slot overrides for the 256-color palette.
///
/// The 16 base ANSI slots are named fields; extended indices `16..=255`
/// live in `indexed`. Missing slots keep the xterm baseline color.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct PaletteConfig {
    /// ANSI slot 0.
    pub black: Option<String>,
    /// ANSI slot 1.
    pub red: Option<String>,
    /// ANSI slot 2.
    pub green: Option<String>,
    /// ANSI slot 3.
    pub yellow: Option<String>,
    /// ANSI slot 4.
    pub blue: Option<String>,
    /// ANSI slot 5.
    pub magenta: Option<String>,
    /// ANSI slot 6.
    pub cyan: Option<String>,
    /// ANSI slot 7.
    pub white: Option<String>,
    /// ANSI slot 8.
    pub bright_black: Option<String>,
    /// ANSI slot 9.
    pub bright_red: Option<String>,
    /// ANSI slot 10.
    pub bright_green: Option<String>,
    /// ANSI slot 11.
    pub bright_yellow: Option<String>,
    /// ANSI slot 12.
    pub bright_blue: Option<String>,
    /// ANSI slot 13.
    pub bright_magenta: Option<String>,
    /// ANSI slot 14.
    pub bright_cyan: Option<String>,
    /// ANSI slot 15.
    pub bright_white: Option<String>,
    /// Overrides for the extended 256-color indices `16..=255` (the
    /// 6×6×6 cube and grayscale ramp), which programs reach through
    /// `SGR 38;5;<n>` / `48;5;<n>`. Slots `0..=15` belong to the named
    /// fields above; any other key is dropped with a warning. Keys are
    /// strings: TOML has no others, so `16` and `"16"` are one key.
    #[cfg_attr(feature = "schema", schemars(with = "BTreeMap<String, String>"))]
    pub indexed: BTreeMap<String, String>,
}

impl PaletteConfig {
    /// Sparse index -> `#rrggbb` map for the renderer's
    /// `Theme::with_palette`. An `indexed` key outside `16..=255` is
    /// dropped (the loader reports it).
    #[must_use]
    pub fn into_overrides(self) -> BTreeMap<u8, String> {
        let named = [
            self.black,
            self.red,
            self.green,
            self.yellow,
            self.blue,
            self.magenta,
            self.cyan,
            self.white,
            self.bright_black,
            self.bright_red,
            self.bright_green,
            self.bright_yellow,
            self.bright_blue,
            self.bright_magenta,
            self.bright_cyan,
            self.bright_white,
        ];
        let named_slots = named.len();
        let mut out: BTreeMap<u8, String> = named
            .into_iter()
            .enumerate()
            .filter_map(|(i, slot)| slot.map(|hex| (i as u8, hex)))
            .collect();
        debug_assert_eq!(named_slots, usize::from(FIRST_INDEXED_SLOT));
        for (key, hex) in self.indexed {
            if let Ok(idx) = indexed_slot(&key) {
                out.insert(idx, hex);
            }
        }
        out
    }
}

const FIRST_INDEXED_SLOT: u8 = 16;

pub(super) enum IndexedSlotError {
    /// `0..=15`, which has a named field.
    NamedSlot,
    NotAnIndex,
}

pub(super) fn indexed_slot(key: &str) -> Result<u8, IndexedSlotError> {
    match key.parse::<u8>() {
        Ok(idx) if idx >= FIRST_INDEXED_SLOT => Ok(idx),
        Ok(_) => Err(IndexedSlotError::NamedSlot),
        Err(_) => Err(IndexedSlotError::NotAnIndex),
    }
}

/// Clipboard backend selection and OSC 52 security gate.
///
/// Uses the OS clipboard for user-initiated paste while keeping OSC 52
/// writes off it by default. Set `osc_52 = "system"` for full integration.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct ClipboardConfig {
    /// When true (default), paste (Ctrl+Shift+V) reads from the OS
    /// clipboard. When false, felis falls back to an in-process
    /// clipboard (useful for headless or sandboxed setups where the
    /// OS clipboard is unreachable).
    pub use_os_clipboard: bool,
    /// Where an OSC 52 set request from the running program lands.
    #[serde(deserialize_with = "deserialize_lenient_enum")]
    pub osc_52: Osc52Policy,
}

/// Where an OSC 52 set request from the running program lands
/// (`clipboard.osc_52`).
// No reject variant: dropping the write breaks the write-then-read
// round-trip a program is entitled to; only the system clipboard needs
// guarding.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Osc52Policy {
    /// The write lands in felis's own mirror and stops there; the system
    /// clipboard is untouched. felis still answers OSC 52 queries from
    /// the mirror, so a program that writes and then reads sees its own
    /// value. Default.
    #[default]
    Mirror,
    /// The write propagates to the system clipboard.
    System,
}

impl Osc52Policy {
    #[must_use]
    pub const fn writes_to_system(self) -> bool {
        matches!(self, Self::System)
    }
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            use_os_clipboard: true,
            osc_52: Osc52Policy::Mirror,
        }
    }
}

/// Font-size band in logical pixels, shared by config load and zoom.
pub const MIN_FONT_SIZE_LOGICAL_PX: f32 = 4.0;
pub const MAX_FONT_SIZE_LOGICAL_PX: f32 = 72.0;

pub(super) enum FontSizeIssue {
    NotPositiveFinite(f32),
    Clamped { raw: f32, clamped: f32 },
}

impl FontConfig {
    /// In logical pixels, like `font.size_px`.
    #[must_use]
    pub fn clamped_font_size(current: f32, delta: f32) -> f32 {
        let next = current + delta;
        next.clamp(MIN_FONT_SIZE_LOGICAL_PX, MAX_FONT_SIZE_LOGICAL_PX)
    }

    /// Logical pixels; the client multiplies by the window's scale factor.
    /// `None` for a non-finite or non-positive size, which the caller
    /// replaces with the renderer's default.
    #[must_use]
    pub fn sanitized_font_size_logical_px(&self) -> Option<f32> {
        let raw = self.size_px?;
        if !raw.is_finite() || raw <= 0.0 {
            return None;
        }
        Some(raw.clamp(MIN_FONT_SIZE_LOGICAL_PX, MAX_FONT_SIZE_LOGICAL_PX))
    }

    pub(super) fn font_size_issue(&self) -> Option<FontSizeIssue> {
        let raw = self.size_px?;
        if !raw.is_finite() || raw <= 0.0 {
            return Some(FontSizeIssue::NotPositiveFinite(raw));
        }
        let clamped = raw.clamp(MIN_FONT_SIZE_LOGICAL_PX, MAX_FONT_SIZE_LOGICAL_PX);
        ((clamped - raw).abs() > f32::EPSILON).then_some(FontSizeIssue::Clamped { raw, clamped })
    }
}

#[must_use]
pub fn compose_window_title(prefix: Option<&str>, shell_title: &str) -> String {
    let prefix = prefix.unwrap_or("").trim();
    if prefix.is_empty() {
        return shell_title.to_owned();
    }
    if shell_title.is_empty() {
        return prefix.to_owned();
    }
    format!("{prefix} {shell_title}")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]

    use crate::action::{Action, ClipboardScope, Escapes, IpcAction, ScrollStep};

    use super::*;

    fn parse_with_ignored(
        text: &str,
        client_id: &str,
    ) -> Result<(EffectiveConfig, Vec<String>), LoadError> {
        let (cfg, diagnostics) = ConfigDocument::parse(text, None)?.resolve(client_id);
        let ignored = diagnostics
            .unknown_keys()
            .into_iter()
            .map(str::to_owned)
            .collect();
        Ok((cfg, ignored))
    }

    #[test]
    fn the_effective_config_round_trips_through_serde() {
        let text = r##"
            [font]
            family = "JetBrains Mono"
            size_px = 13.5
            features = ["calt"]
            fallback = [{ family = "Noto Sans CJK JP" }]

            [theme]
            background = "#101010"

            [theme.palette]
            red = "#ff5555"

            [cursor]
            blink = "never"

            [shader]
            post = { builtin = "trail" }
            animate = "focused"

            [keymap]
            "ctrl+shift+f" = "open_scrollback_search"

            [client.other]
            anything = 1
        "##;
        let (cfg, _diagnostics) = ConfigDocument::parse(text, None).unwrap().resolve("felis");
        let json = serde_json::to_string(&cfg).unwrap();
        let back: EffectiveConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cfg);
        assert!(json.contains("\"other\""), "{json}");
    }

    #[test]
    fn a_broken_section_for_another_client_is_inert() {
        let text = r#"
            [font]
            size_px = 13.0

            [client.other]
            size_px = "not a number"
            totally_unknown = true

            [client.other.font]
            family = 7
        "#;
        let (cfg, diagnostics) = ConfigDocument::parse(text, None).unwrap().resolve("felis");
        assert_eq!(cfg.font.size_px, Some(13.0));
        assert!(!diagnostics.has_errors());
        assert_eq!(diagnostics.unknown_keys(), Vec::<&str>::new());
        assert_eq!(diagnostics.iter().count(), 0);
    }

    #[test]
    fn empty_string_parses_to_default() {
        let cfg: EffectiveConfig = toml::from_str("").unwrap();
        assert_eq!(cfg, EffectiveConfig::default());
        assert_eq!(
            cfg.keymap.compile(None),
            Vec::<(Chord, BindingValue)>::new()
        );
    }

    #[test]
    fn absent_shader_section_means_no_post_pass() {
        let cfg: EffectiveConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.shader.post, None);
        assert_eq!(cfg.post_shader_choice(), PostShaderChoice::None);
    }

    #[test]
    fn animation_is_off_unless_asked_for() {
        let cfg: EffectiveConfig =
            toml::from_str("[shader]\npost = { builtin = \"trail\" }\n").unwrap();
        assert_eq!(cfg.shader.animate, ShaderAnimation::Never);
        let cfg: EffectiveConfig = toml::from_str("[shader]\nanimate = \"focused\"\n").unwrap();
        assert_eq!(cfg.shader.animate, ShaderAnimation::Focused);
    }

    #[test]
    fn the_builtin_table_selects_a_shipped_shader() {
        let cfg: EffectiveConfig =
            toml::from_str("[shader]\npost = { builtin = \"trail\" }\n").unwrap();
        assert_eq!(
            cfg.post_shader_choice(),
            PostShaderChoice::Builtin(BuiltinShader::Trail),
        );
    }

    #[test]
    fn a_file_path_resolves_against_the_config_file() {
        let cfg: EffectiveConfig =
            toml::from_str("[shader]\npost = { file = \"/opt/fx/glow.wgsl\" }\n").unwrap();
        assert_eq!(
            cfg.post_shader_choice(),
            PostShaderChoice::File(PathBuf::from("/opt/fx/glow.wgsl")),
        );
        let cfg: EffectiveConfig =
            toml::from_str("[shader]\npost = { file = \"fx/glow.wgsl\" }\n").unwrap();
        assert_eq!(
            cfg.post_shader_choice(),
            PostShaderChoice::File(PathBuf::from("fx/glow.wgsl")),
            "with no config file to be relative to, the path stays as written",
        );
        let document = ConfigDocument::parse(
            "[shader]\npost = { file = \"fx/glow.wgsl\" }\n",
            Some("/home/u/.config/felis/config.toml".into()),
        )
        .unwrap();
        let (cfg, _) = document.resolve("felis");
        assert_eq!(
            cfg.post_shader_choice(),
            PostShaderChoice::File(PathBuf::from("/home/u/.config/felis/fx/glow.wgsl")),
        );
    }

    #[test]
    fn a_bare_string_is_not_a_shader_value() {
        assert!(toml::from_str::<EffectiveConfig>("[shader]\npost = \"trail\"\n").is_err());
        assert!(toml::from_str::<EffectiveConfig>("[shader]\npost = \"glow.wgsl\"\n").is_err());
    }

    #[test]
    fn a_malformed_shader_selector_payload_is_still_a_type_error() {
        assert!(
            toml::from_str::<EffectiveConfig>("[shader]\npost = { file = 3 }\n").is_err(),
            "a known selector with the wrong payload shape is malformed, not futuristic",
        );
        assert!(toml::from_str::<EffectiveConfig>("[shader]\npost = { builtin = 3 }\n").is_err());
    }

    #[test]
    fn an_unknown_builtin_degrades_to_no_post_pass_with_a_warning() {
        let (cfg, diagnostics) =
            ConfigDocument::parse("[shader]\npost = { builtin = \"trial\" }\n", None)
                .unwrap()
                .resolve("felis");
        assert_eq!(cfg.shader.post, None);
        assert!(!diagnostics.has_errors(), "{diagnostics:?}");
        let warning = only_warning(&diagnostics);
        assert_eq!(warning.kind, DiagnosticKind::Value);
        assert_eq!(warning.key.as_deref(), Some("shader.post.builtin"));
        assert!(warning.message.contains("trial"), "{warning:?}");
    }

    #[test]
    fn an_unknown_shader_selector_degrades_to_no_post_pass_with_a_warning() {
        let (cfg, diagnostics) =
            ConfigDocument::parse("[shader]\npost = { procedural = \"rain\" }\n", None)
                .unwrap()
                .resolve("felis");
        assert_eq!(cfg.shader.post, None);
        assert!(!diagnostics.has_errors(), "{diagnostics:?}");
        let warning = only_warning(&diagnostics);
        assert_eq!(warning.key.as_deref(), Some("shader.post"));
        assert!(warning.message.contains("procedural"), "{warning:?}");
    }

    /// The whole point of the per-field rule: the rest of the document
    /// survives a token this build has never heard of.
    #[test]
    fn an_unknown_enum_token_keeps_every_other_setting() {
        let (cfg, diagnostics) = ConfigDocument::parse(
            r#"
            [font]
            family = "JetBrains Mono"
            size_px = 13.0

            [cursor]
            blink = "breathe"
            blink_interval_ms = 120

            [clipboard]
            osc_52 = "system"
        "#,
            None,
        )
        .unwrap()
        .resolve("felis");
        assert_eq!(cfg.font.family.as_deref(), Some("JetBrains Mono"));
        assert_eq!(cfg.font.size_px, Some(13.0));
        assert_eq!(cfg.cursor.blink_interval_ms, 120);
        assert_eq!(cfg.clipboard.osc_52, Osc52Policy::System);
        assert_eq!(cfg.cursor.blink, CursorBlinkMode::Program);
        assert!(!diagnostics.has_errors(), "{diagnostics:?}");
        assert_eq!(
            only_warning(&diagnostics).key.as_deref(),
            Some("cursor.blink")
        );
    }

    #[test]
    fn every_closed_enum_degrades_to_its_default_with_the_full_path() {
        for (text, key) in [
            ("[cursor]\nblink = \"breathe\"\n", "cursor.blink"),
            ("[shader]\nanimate = \"always\"\n", "shader.animate"),
            ("[window]\nbackdrop = \"vibrancy\"\n", "window.backdrop"),
            ("[clipboard]\nosc_52 = \"reject\"\n", "clipboard.osc_52"),
            (
                "[shader]\npost = { builtin = \"rain\" }\n",
                "shader.post.builtin",
            ),
            ("[shader]\npost = { chain = \"a\" }\n", "shader.post"),
        ] {
            let (cfg, diagnostics) = ConfigDocument::parse(text, None).unwrap().resolve("felis");
            assert_eq!(cfg, EffectiveConfig::default(), "{key}");
            assert!(!diagnostics.has_errors(), "{key}: {diagnostics:?}");
            let warning = only_warning(&diagnostics);
            assert_eq!(warning.kind, DiagnosticKind::Value, "{key}");
            assert_eq!(warning.key.as_deref(), Some(key));
        }
    }

    #[test]
    fn an_unknown_enum_token_in_the_overlay_reports_the_overlay_path() {
        let (cfg, diagnostics) = ConfigDocument::parse(
            "[window]\nbackdrop = \"blur\"\n\n[client.felis.window]\nbackdrop = \"vibrancy\"\n",
            None,
        )
        .unwrap()
        .resolve("felis");
        assert_eq!(cfg.window.backdrop, Backdrop::None);
        assert!(!diagnostics.has_errors(), "{diagnostics:?}");
        assert_eq!(
            only_warning(&diagnostics).key.as_deref(),
            Some("client.felis.window.backdrop"),
        );
    }

    #[test]
    fn a_known_token_in_the_overlay_leaves_no_warning() {
        let (cfg, diagnostics) = ConfigDocument::parse(
            "[window]\nbackdrop = \"blur\"\n\n[client.felis.window]\nbackdrop = \"mica\"\n",
            None,
        )
        .unwrap()
        .resolve("felis");
        assert_eq!(cfg.window.backdrop, Backdrop::Mica);
        assert_eq!(diagnostics.iter().count(), 0, "{diagnostics:?}");
    }

    fn only_warning(diagnostics: &ConfigDiagnostics) -> Diagnostic {
        let warnings: Vec<_> = diagnostics.warnings().cloned().collect();
        assert_eq!(warnings.len(), 1, "{diagnostics:?}");
        warnings.into_iter().next().unwrap()
    }

    #[test]
    fn keymap_section_parses_single_entry() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [keymap]
            "ctrl+shift+f" = { kind = "open_scrollback_search" }
        "#,
        )
        .unwrap();
        let compiled = cfg.keymap.compile(None);
        assert_eq!(compiled.len(), 1);
        let (chord, binding) = compiled.into_iter().next().unwrap();
        assert_eq!(chord, "ctrl+shift+f".parse().unwrap());
        assert_eq!(binding, BindingValue::OpenScrollbackSearch);
        assert_eq!(
            binding.into_action(),
            Some(Action::Ipc(IpcAction::OpenScrollbackSearch)),
        );
    }

    #[test]
    fn keymap_section_parses_spawn_session() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [keymap]
            "ctrl+shift+n" = { kind = "new_session" }
        "#,
        )
        .unwrap();
        let compiled = cfg.keymap.compile(None);
        assert_eq!(compiled.len(), 1);
        let (chord, binding) = compiled.into_iter().next().unwrap();
        assert_eq!(chord, "ctrl+shift+n".parse().unwrap());
        assert_eq!(binding, BindingValue::NewSession);
        assert_eq!(
            binding.into_action(),
            Some(Action::Ipc(IpcAction::NewSession)),
        );
    }

    #[test]
    fn keymap_struct_variants_carry_typed_args() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [keymap]
            "ctrl+shift+v" = { kind = "paste", from = "system" }
            "shift+insert" = { kind = "paste", from = "primary" }
            "ctrl+shift+s" = { kind = "send_string", text = "hello\n" }
            "shift+page_up" = { kind = "scroll", step = "half_page_up" }
        "#,
        )
        .unwrap();
        let compiled: BTreeMap<Chord, BindingValue> =
            cfg.keymap.compile(None).into_iter().collect();

        assert_eq!(
            compiled.get(&"ctrl+shift+v".parse().unwrap()),
            Some(&BindingValue::Paste {
                from: ClipboardScope::System,
            }),
        );
        assert_eq!(
            compiled.get(&"shift+insert".parse().unwrap()),
            Some(&BindingValue::Paste {
                from: ClipboardScope::Primary,
            }),
        );
        assert_eq!(
            compiled.get(&"ctrl+shift+s".parse().unwrap()),
            Some(&BindingValue::SendString {
                text: "hello\n".into(),
                escapes: Escapes::CStyle,
            }),
        );
        assert_eq!(
            compiled.get(&"shift+page_up".parse().unwrap()),
            Some(&BindingValue::Scroll {
                step: ScrollStep::HalfPageUp,
            }),
        );
    }

    #[test]
    fn keymap_parses_pipe_binding_default_and_explicit_command() {
        use crate::action::{PipeRegionSource, PipeTarget};
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [keymap]
            "ctrl+shift+h" = { kind = "pipe", source = "scrollback" }
            "ctrl+shift+g" = { kind = "pipe", source = "visible", target = { command = ["bat", "-l", "log"] }, ansi = true }
            "ctrl+shift+s" = { kind = "pipe", source = "selection", target = { command = ["wl-copy"] } }
        "#,
        )
        .unwrap();
        let compiled: BTreeMap<Chord, BindingValue> =
            cfg.keymap.compile(None).into_iter().collect();

        assert_eq!(
            compiled
                .get(&"ctrl+shift+h".parse().unwrap())
                .unwrap()
                .clone()
                .into_action(),
            Some(Action::PipeRegion {
                source: PipeRegionSource::Scrollback,
                target: PipeTarget::Command(Vec::new()),
                ansi: false,
            }),
        );
        assert_eq!(
            compiled
                .get(&"ctrl+shift+g".parse().unwrap())
                .unwrap()
                .clone()
                .into_action(),
            Some(Action::PipeRegion {
                source: PipeRegionSource::Visible,
                target: PipeTarget::Command(vec!["bat".into(), "-l".into(), "log".into()]),
                ansi: true,
            }),
        );
        assert_eq!(
            compiled
                .get(&"ctrl+shift+s".parse().unwrap())
                .unwrap()
                .clone()
                .into_action(),
            Some(Action::PipeRegion {
                source: PipeRegionSource::Selection,
                target: PipeTarget::Command(vec!["wl-copy".into()]),
                ansi: false,
            }),
        );
    }

    #[test]
    fn keymap_parses_each_pipe_target_token() {
        use crate::action::{PipeRegionSource, PipeTarget};
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [keymap]
            "ctrl+shift+c" = { kind = "pipe", source = "selection", target = "clipboard" }
            "ctrl+shift+v" = { kind = "pipe", source = "selection", target = "paste" }
            "ctrl+shift+f" = { kind = "pipe", source = "scrollback", target = "temp_file" }
            "ctrl+shift+o" = { kind = "pipe", source = "scrollback", target = { file = "/tmp/felis.log" } }
        "#,
        )
        .unwrap();
        let compiled: BTreeMap<Chord, BindingValue> =
            cfg.keymap.compile(None).into_iter().collect();

        assert_eq!(
            compiled
                .get(&"ctrl+shift+c".parse().unwrap())
                .unwrap()
                .clone()
                .into_action(),
            Some(Action::PipeRegion {
                source: PipeRegionSource::Selection,
                target: PipeTarget::Clipboard,
                ansi: false,
            }),
        );
        assert_eq!(
            compiled
                .get(&"ctrl+shift+v".parse().unwrap())
                .unwrap()
                .clone()
                .into_action(),
            Some(Action::PipeRegion {
                source: PipeRegionSource::Selection,
                target: PipeTarget::Paste,
                ansi: false,
            }),
        );
        assert_eq!(
            compiled
                .get(&"ctrl+shift+f".parse().unwrap())
                .unwrap()
                .clone()
                .into_action(),
            Some(Action::PipeRegion {
                source: PipeRegionSource::Scrollback,
                target: PipeTarget::File(None),
                ansi: false,
            }),
        );
        assert_eq!(
            compiled
                .get(&"ctrl+shift+o".parse().unwrap())
                .unwrap()
                .clone()
                .into_action(),
            Some(Action::PipeRegion {
                source: PipeRegionSource::Scrollback,
                target: PipeTarget::File(Some("/tmp/felis.log".into())),
                ansi: false,
            }),
        );
    }

    #[test]
    fn keymap_pipe_payload_on_a_payload_free_sink_does_not_deserialize() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [keymap]
            "ctrl+shift+c" = { kind = "pipe", source = "selection", target = "clipboard", command = ["oops"] }
            "ctrl+shift+r" = { kind = "reload" }
        "#,
        )
        .unwrap();
        let compiled: BTreeMap<Chord, BindingValue> =
            cfg.keymap.compile(None).into_iter().collect();
        assert!(
            !compiled.contains_key(&"ctrl+shift+c".parse().unwrap()),
            "a clipboard pipe carrying a `command` argv must be dropped",
        );
        assert_eq!(
            compiled.get(&"ctrl+shift+r".parse().unwrap()),
            Some(&BindingValue::Reload),
            "the sibling entry survives the drop",
        );
    }

    #[test]
    fn documented_send_string_bindings_decode_to_their_advertised_bytes() {
        // Pins the snippets in how-to/fix-keyboard-input-problems.md.
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [keymap]
            "¥" = { kind = "send_string", text = '\', escapes = "none" }
            "shift+enter" = { kind = "send_string", text = '\e\r' }
        "#,
        )
        .unwrap();
        let compiled: BTreeMap<Chord, BindingValue> =
            cfg.keymap.compile(None).into_iter().collect();
        let decoded = |chord: &str| match compiled.get(&chord.parse::<Chord>().unwrap()) {
            Some(BindingValue::SendString { text, escapes }) => escapes.decode(text).unwrap(),
            other => panic!("expected a send_string binding, got {other:?}"),
        };
        assert_eq!(decoded("¥"), b"\\", "the yen key must emit one backslash");
        assert_eq!(
            decoded("shift+enter"),
            b"\x1b\r",
            "shift+enter must emit the Option+Enter sequence",
        );
    }

    #[test]
    fn keymap_send_string_with_unknown_escape_is_dropped() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [keymap]
            "ctrl+shift+a" = { kind = "send_string", text = '\q', escapes = "c_style" }
            "ctrl+shift+b" = { kind = "send_string", text = '\q', escapes = "none" }
        "#,
        )
        .unwrap();
        let compiled: BTreeMap<Chord, BindingValue> =
            cfg.keymap.compile(None).into_iter().collect();
        assert!(
            !compiled.contains_key(&"ctrl+shift+a".parse().unwrap()),
            "an unknown c_style escape must drop the entry",
        );
        assert!(
            compiled.contains_key(&"ctrl+shift+b".parse().unwrap()),
            "escapes = \"none\" has no escape grammar to violate",
        );
    }

    #[test]
    fn keymap_pipe_sink_takes_only_its_own_payload() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [keymap]
            "ctrl+shift+p" = { kind = "pipe", source = "scrollback", target = { command = "/tmp/x" } }
            "ctrl+shift+q" = { kind = "pipe", source = "scrollback", target = "command" }
            "ctrl+shift+r" = { kind = "reload" }
        "#,
        )
        .unwrap();
        let compiled: BTreeMap<Chord, BindingValue> =
            cfg.keymap.compile(None).into_iter().collect();
        assert!(
            !compiled.contains_key(&"ctrl+shift+p".parse().unwrap()),
            "a command sink carrying a path must be dropped",
        );
        assert!(
            !compiled.contains_key(&"ctrl+shift+q".parse().unwrap()),
            "the command sink must carry its argv",
        );
        assert_eq!(
            compiled.get(&"ctrl+shift+r".parse().unwrap()),
            Some(&BindingValue::Reload),
        );
    }

    #[test]
    fn keymap_parses_run_binding_without_a_source() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [keymap]
            "ctrl+shift+p" = { kind = "run", command = ["felis-session-picker"] }
        "#,
        )
        .unwrap();
        let compiled: BTreeMap<Chord, BindingValue> =
            cfg.keymap.compile(None).into_iter().collect();
        assert_eq!(
            compiled
                .get(&"ctrl+shift+p".parse().unwrap())
                .unwrap()
                .clone()
                .into_action(),
            Some(Action::RunCommand {
                command: vec!["felis-session-picker".into()],
            }),
        );
    }

    #[test]
    fn keymap_run_without_a_command_is_dropped() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [keymap]
            "ctrl+shift+p" = { kind = "run" }
        "#,
        )
        .unwrap();
        let compiled: BTreeMap<Chord, BindingValue> =
            cfg.keymap.compile(None).into_iter().collect();
        assert!(
            !compiled.contains_key(&"ctrl+shift+p".parse().unwrap()),
            "a `run` binding missing its required `command` must be dropped",
        );
    }

    #[test]
    fn keymap_run_with_an_empty_command_is_dropped() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [keymap]
            "ctrl+shift+p" = { kind = "run", command = [] }
            "ctrl+shift+r" = { kind = "reload" }
        "#,
        )
        .unwrap();
        let compiled: BTreeMap<Chord, BindingValue> =
            cfg.keymap.compile(None).into_iter().collect();
        assert!(
            !compiled.contains_key(&"ctrl+shift+p".parse().unwrap()),
            "a `run` binding with an empty `command` must be dropped",
        );
        assert_eq!(
            compiled.get(&"ctrl+shift+r".parse().unwrap()),
            Some(&BindingValue::Reload),
        );
    }

    #[test]
    fn keymap_unbind_is_a_first_class_variant() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [keymap]
            "ctrl+shift+r" = { kind = "unbind" }
        "#,
        )
        .unwrap();
        let compiled: Vec<_> = cfg.keymap.compile(None);
        assert_eq!(compiled.len(), 1);
        assert_eq!(compiled[0].1, BindingValue::Unbind);
        assert_eq!(compiled[0].1.clone().into_action(), None);
    }

    #[test]
    fn keymap_malformed_chord_skips_entry_keeps_rest() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [keymap]
            "meta+a"        = { kind = "reload" }
            "ctrl+shift+r"  = { kind = "reload" }
        "#,
        )
        .unwrap();
        let compiled = cfg.keymap.compile(None);
        assert_eq!(compiled.len(), 1);
        assert_eq!(compiled[0].0, "ctrl+shift+r".parse().unwrap());
    }

    #[test]
    fn keymap_unknown_binding_kind_skips_entry_keeps_rest() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [keymap]
            "ctrl+shift+x" = { kind = "burn_screen" }
            "ctrl+shift+r" = { kind = "reload" }
        "#,
        )
        .unwrap();
        let compiled = cfg.keymap.compile(None);
        assert_eq!(compiled.len(), 1);
        assert_eq!(compiled[0].1, BindingValue::Reload);
    }

    #[test]
    fn keymap_malformed_binding_args_skips_entry_keeps_rest() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [keymap]
            "ctrl+shift+v" = { kind = "paste" }
            "ctrl+shift+r" = { kind = "reload" }
        "#,
        )
        .unwrap();
        let compiled = cfg.keymap.compile(None);
        assert_eq!(compiled.len(), 1);
        assert_eq!(compiled[0].1, BindingValue::Reload);
    }

    #[test]
    fn keymap_rejects_unknown_top_level_arg() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [keymap]
            "ctrl+shift+v" = { kind = "paste", target = "system" }
        "#,
        )
        .unwrap();
        assert_eq!(
            cfg.keymap.compile(None),
            Vec::<(Chord, BindingValue)>::new()
        );
    }

    #[test]
    fn keymap_section_does_not_interfere_with_other_sections() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [font]
            family = "Source Code Pro"
            size_px = 14.0

            [keymap]
            "ctrl+shift+r" = { kind = "reload" }
        "#,
        )
        .unwrap();
        assert_eq!(cfg.font.family.as_deref(), Some("Source Code Pro"));
        assert_eq!(cfg.font.size_px, Some(14.0));
        let compiled = cfg.keymap.compile(None);
        assert_eq!(compiled.len(), 1);
        assert_eq!(compiled[0].1, BindingValue::Reload);
    }

    #[test]
    fn keymap_top_level_typo_is_reported_not_fatal() {
        let (cfg, ignored) = parse_with_ignored(
            r#"
            [kemyap]
            "ctrl+r" = { kind = "reload" }
        "#,
            "felis",
        )
        .unwrap();
        assert_eq!(cfg, EffectiveConfig::default());
        assert_eq!(ignored, vec!["kemyap".to_owned()]);
    }

    #[test]
    fn font_family_and_size_round_trip() {
        let toml_src = r#"
            [font]
            family = "JetBrains Mono"
            size_px = 14.0
        "#;
        let cfg: EffectiveConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(cfg.font.family.as_deref(), Some("JetBrains Mono"));
        assert_eq!(cfg.font.size_px, Some(14.0));
        assert_eq!(cfg.font.fallback, Vec::<FontStyleConfig>::new());
    }

    fn fallback_families(cfg: &EffectiveConfig) -> Vec<&str> {
        cfg.font
            .fallback
            .iter()
            .map(|entry| entry.family.as_deref().expect("family is required"))
            .collect()
    }

    #[test]
    fn font_fallback_list_parses_in_declared_order() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [font]
            family = "JetBrainsMono Nerd Font"
            fallback = [
                { family = "Noto Sans CJK JP" },
                { family = "Noto Color Emoji" },
            ]
        "#,
        )
        .unwrap();
        assert_eq!(cfg.font.family.as_deref(), Some("JetBrainsMono Nerd Font"));
        assert_eq!(
            fallback_families(&cfg),
            ["Noto Sans CJK JP", "Noto Color Emoji"]
        );
        assert_eq!(cfg.font.fallback[0].features, None);
    }

    #[test]
    fn font_features_primary_list_parses() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [font]
            family = "FiraCode"
            features = ["calt", "liga", "ss01", "-clig"]
        "#,
        )
        .unwrap();
        assert_eq!(
            cfg.font.features,
            vec!["calt", "liga", "ss01", "-clig"]
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn font_features_defaults_to_empty() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [font]
            family = "FiraCode"
        "#,
        )
        .unwrap();
        assert_eq!(cfg.font.features, Vec::<String>::new());
    }

    #[test]
    fn font_fallback_features_override_parses() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [font]
            family = "FiraCode"
            features = ["calt"]
            fallback = [
                { family = "Noto Sans CJK JP", features = ["palt"] },
            ]
        "#,
        )
        .unwrap();
        assert_eq!(fallback_families(&cfg), ["Noto Sans CJK JP"]);
        assert_eq!(
            cfg.font.fallback[0].features.as_deref(),
            Some(["palt".to_owned()].as_slice())
        );
    }

    #[test]
    fn font_fallback_omitted_features_inherits_empty_opts_out() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [font]
            features = ["calt"]
            fallback = [
                { family = "Symbols Nerd Font" },
                { family = "Noto Color Emoji", features = [] },
            ]
        "#,
        )
        .unwrap();
        assert_eq!(
            cfg.font.fallback[0].features, None,
            "an omitted features list inherits the primary's",
        );
        let empty: &[String] = &[];
        assert_eq!(
            cfg.font.fallback[1].features.as_deref(),
            Some(empty),
            "an explicit empty list opts out — color-emoji must not inherit `calt`",
        );
    }

    #[test]
    fn font_fallback_entry_with_stray_key_skips_entry_keeps_rest() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [font]
            fallback = [
                { family = "Noto Sans CJK JP" },
                { familly = "Typo Sans" },
                { family = "Noto Color Emoji" },
            ]
        "#,
        )
        .unwrap();
        assert_eq!(
            fallback_families(&cfg),
            ["Noto Sans CJK JP", "Noto Color Emoji"],
            "the stray-key entry drops; the two valid ones survive in order",
        );
    }

    #[test]
    fn font_fallback_entry_without_family_skips_entry_keeps_rest() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [font]
            fallback = [
                { family = "Noto Sans CJK JP" },
                { features = ["calt"] },
                { family = "Noto Color Emoji" },
            ]
        "#,
        )
        .unwrap();
        assert_eq!(
            fallback_families(&cfg),
            ["Noto Sans CJK JP", "Noto Color Emoji"],
        );
    }

    #[test]
    fn font_fallback_bare_string_entry_skips_entry_keeps_rest() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [font]
            fallback = [
                { family = "Noto Sans CJK JP" },
                "Noto Color Emoji",
            ]
        "#,
        )
        .unwrap();
        assert_eq!(fallback_families(&cfg), ["Noto Sans CJK JP"]);
    }

    #[test]
    fn font_fallback_wrong_type_skips_entry_keeps_rest() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [font]
            fallback = [
                { family = "Noto Sans CJK JP" },
                42,
                { family = "Noto Color Emoji" },
            ]
        "#,
        )
        .unwrap();
        assert_eq!(
            fallback_families(&cfg),
            ["Noto Sans CJK JP", "Noto Color Emoji"],
        );
    }

    #[test]
    fn window_title_prefix_parses() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [window]
            title_prefix = "[work]"
        "#,
        )
        .unwrap();
        assert_eq!(cfg.window.title_prefix.as_deref(), Some("[work]"));
    }

    #[test]
    fn window_decorations_defaults_true() {
        let cfg = EffectiveConfig::default();
        assert!(cfg.window.decorations);
        let cfg: EffectiveConfig = toml::from_str("[window]").unwrap();
        assert!(cfg.window.decorations);
    }

    #[test]
    fn window_decorations_false_parses() {
        let cfg: EffectiveConfig = toml::from_str(
            r"
            [window]
            decorations = false
        ",
        )
        .unwrap();
        assert!(!cfg.window.decorations);
    }

    #[test]
    fn window_opacity_defaults_fully_opaque() {
        assert_eq!(EffectiveConfig::default().window.opacity, 1.0);
        let cfg: EffectiveConfig = toml::from_str("[window]").unwrap();
        assert_eq!(cfg.window.opacity, 1.0);
        assert_eq!(cfg.window.clamped_opacity(), 1.0);
    }

    #[test]
    fn window_opacity_parses_and_reports_translucent() {
        let cfg: EffectiveConfig = toml::from_str(
            r"
            [window]
            opacity = 0.85
        ",
        )
        .unwrap();
        assert!((cfg.window.clamped_opacity() - 0.85).abs() < f32::EPSILON);
        assert!(cfg.window.clamped_opacity() < 1.0);
    }

    /// NaN is outside every band, so it is replaced rather than clamped:
    /// `f32::clamp` would pass it through and a NaN opacity renders
    /// nothing.
    #[test]
    fn a_nan_knob_falls_back_instead_of_clamping() {
        let nan = WindowConfig {
            opacity: f32::NAN,
            ..WindowConfig::default()
        };
        assert_eq!(nan.clamped_opacity(), 1.0);
        let nan = MouseConfig {
            scroll_multiplier: f64::NAN,
        };
        assert_eq!(nan.clamped_scroll_multiplier(), DEFAULT_SCROLL_MULTIPLIER);
    }

    #[test]
    fn window_backdrop_defaults_off() {
        assert_eq!(EffectiveConfig::default().window.backdrop, Backdrop::None);
        let cfg: EffectiveConfig = toml::from_str("[window]").unwrap();
        assert_eq!(cfg.window.backdrop, Backdrop::None);
    }

    #[test]
    fn window_backdrop_parses_each_material() {
        for (toml_value, expected) in [
            ("none", Backdrop::None),
            ("blur", Backdrop::Blur),
            ("acrylic", Backdrop::Acrylic),
            ("mica", Backdrop::Mica),
            ("tabbed", Backdrop::Tabbed),
        ] {
            let cfg: EffectiveConfig =
                toml::from_str(&format!("[window]\nbackdrop = \"{toml_value}\"\n")).unwrap();
            assert_eq!(cfg.window.backdrop, expected);
        }
    }

    #[test]
    fn window_blur_with_opacity_parses() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [window]
            opacity = 0.8
            backdrop = "blur"
        "#,
        )
        .unwrap();
        assert_eq!(cfg.window.backdrop, Backdrop::Blur);
        assert!(cfg.window.clamped_opacity() < 1.0);
    }

    #[test]
    fn window_title_bar_glass_combo_parses() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [window]
            backdrop = "mica"
            opacity = 1.0
        "#,
        )
        .unwrap();
        assert_eq!(cfg.window.backdrop, Backdrop::Mica);
        assert_eq!(cfg.window.clamped_opacity(), 1.0);
    }

    #[test]
    fn window_section_unknown_field_reported_with_full_path() {
        let (cfg, ignored) = parse_with_ignored(
            r#"
            [window]
            prefix = "[work]"
            decorations = false
        "#,
            "felis",
        )
        .unwrap();
        assert_eq!(ignored, vec!["window.prefix".to_owned()]);
        assert!(!cfg.window.decorations);
        assert_eq!(cfg.window.title_prefix, None);
    }

    #[test]
    fn cursor_color_parses() {
        let cfg: EffectiveConfig = toml::from_str(
            r##"
            [cursor]
            color = "#ffaa00"
        "##,
        )
        .unwrap();
        assert_eq!(cfg.cursor.color.as_deref(), Some("#ffaa00"));
        assert_eq!(cfg.cursor.blink, CursorBlinkMode::Program);
        assert_eq!(cfg.cursor.blink_interval_ms, 530);
    }

    #[test]
    fn cursor_blink_section_parses_every_mode_and_the_interval() {
        for (spelling, want) in [
            ("program", CursorBlinkMode::Program),
            ("never", CursorBlinkMode::Never),
            ("always", CursorBlinkMode::Always),
        ] {
            let cfg: EffectiveConfig =
                toml::from_str(&format!("[cursor]\nblink = \"{spelling}\"\n")).unwrap();
            assert_eq!(cfg.cursor.blink, want, "blink = {spelling:?}");
            assert_eq!(
                cfg.cursor.blink_interval_ms, 530,
                "the interval keeps its default when only the mode is set",
            );
        }
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [cursor]
            blink = "always"
            blink_interval_ms = 120
        "#,
        )
        .unwrap();
        assert_eq!(cfg.cursor.blink, CursorBlinkMode::Always);
        assert_eq!(cfg.cursor.blink_interval_ms, 120);
        // `BlinkClock` owns the busy-loop floor; `0` is stored as written.
        let cfg: EffectiveConfig = toml::from_str("[cursor]\nblink_interval_ms = 0\n").unwrap();
        assert_eq!(cfg.cursor.blink_interval_ms, 0);
    }

    #[test]
    fn cursor_blink_degrades_an_unknown_mode_with_a_warning() {
        let (cfg, diagnostics) = ConfigDocument::parse("[cursor]\nblink = \"alway\"\n", None)
            .unwrap()
            .resolve("felis");
        assert_eq!(cfg.cursor.blink, CursorBlinkMode::Program);
        assert!(!diagnostics.has_errors(), "{diagnostics:?}");
        let warning = only_warning(&diagnostics);
        assert_eq!(warning.kind, DiagnosticKind::Value);
        assert_eq!(warning.key.as_deref(), Some("cursor.blink"));
        assert!(warning.message.contains("alway"), "{warning:?}");
    }

    #[test]
    fn cursor_blink_rejects_a_value_of_the_wrong_shape() {
        assert!(toml::from_str::<EffectiveConfig>("[cursor]\nblink = 3\n").is_err());
        assert!(
            toml::from_str::<EffectiveConfig>("[cursor]\nblink = { mode = \"never\" }\n").is_err()
        );
    }

    #[test]
    fn mouse_scroll_multiplier_parses_and_defaults_to_three() {
        let cfg: EffectiveConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.mouse.scroll_multiplier, 3.0, "absent section");
        let cfg: EffectiveConfig = toml::from_str("[mouse]\nscroll_multiplier = 7.5\n").unwrap();
        assert_eq!(cfg.mouse.scroll_multiplier, 7.5);
        assert_eq!(cfg.mouse.clamped_scroll_multiplier(), 7.5);
    }

    #[test]
    fn theme_foreground_background_parse() {
        let cfg: EffectiveConfig = toml::from_str(
            r##"
            [theme]
            foreground = "#cdcdcd"
            background = "#101010"
        "##,
        )
        .unwrap();
        assert_eq!(cfg.theme.foreground.as_deref(), Some("#cdcdcd"));
        assert_eq!(cfg.theme.background.as_deref(), Some("#101010"));
    }

    #[test]
    fn missing_section_keeps_other_section_default() {
        let cfg: EffectiveConfig = toml::from_str(r"font = { size_px = 12.0 }").unwrap();
        assert_eq!(cfg.font.size_px, Some(12.0));
        assert_eq!(cfg.theme, ThemeConfig::default());
    }

    #[test]
    fn unknown_top_level_key_is_reported_so_typos_surface() {
        let (cfg, ignored) = parse_with_ignored(r"fnt = { size_px = 12.0 }", "felis").unwrap();
        assert_eq!(cfg, EffectiveConfig::default());
        assert_eq!(ignored, vec!["fnt".to_owned()]);
    }

    #[test]
    fn an_empty_file_resolves_equal_to_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        let cfg = EffectiveConfig::try_load_from(&path, "felis").unwrap();
        assert_eq!(cfg.source_dir.as_deref(), Some(dir.path()));
        assert_eq!(cfg, EffectiveConfig::default());
    }

    #[test]
    fn a_type_error_still_reports_the_rest_of_the_document() {
        let text = "fnt = { size_px = 12.0 }\n\
                    [font]\n\
                    size_px = \"big\"\n\
                    fallback = [12]\n";
        let (_cfg, diagnostics) = ConfigDocument::parse(text, None).unwrap().resolve("felis");
        assert!(diagnostics.has_errors(), "the type error is still an error");
        assert_eq!(diagnostics.unknown_keys(), ["fnt"]);
        assert!(
            diagnostics
                .warnings()
                .any(|d| d.key.as_deref() == Some("font.fallback[0]")),
            "got {:?}",
            diagnostics.iter().collect::<Vec<_>>(),
        );
    }

    #[test]
    fn try_load_from_missing_file_returns_ok_default() {
        let path = PathBuf::from("/definitely/not/a/real/path/config.toml");
        let outcome = EffectiveConfig::try_load_from(&path, "felis");
        assert!(matches!(outcome, Ok(ref cfg) if cfg == &EffectiveConfig::default()));
    }

    /// A file the user named by hand and a file platform discovery
    /// guessed at are different questions, so an absent one gets
    /// different answers.
    #[test]
    fn an_absent_file_is_an_error_only_when_it_was_selected() {
        let dir = tempfile::tempdir().unwrap();
        let selected = ConfigSource::Explicit(dir.path().join("nowhere.toml"));

        let (config, diagnostics) = EffectiveConfig::diagnose_source(&selected, "felis");
        assert_eq!(config, EffectiveConfig::default());
        let errors: Vec<_> = diagnostics.errors().collect();
        assert_eq!(errors.len(), 1, "{diagnostics:?}");
        assert_eq!(errors[0].kind, DiagnosticKind::MissingFile);
        assert!(
            EffectiveConfig::try_load_from_source(&selected, "felis").is_err(),
            "a selected file that is not there must not load as the defaults",
        );

        // The default source keeps reaching platform discovery, whose
        // absent file stays the documented use-the-defaults case
        // (`try_load_from_missing_file_returns_ok_default`).
        assert_eq!(ConfigSource::Default.path(), config_path());
    }

    #[test]
    fn try_load_from_unparseable_file_returns_invalid_err() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "this = is = not = toml").unwrap();
        let outcome = EffectiveConfig::try_load_from(&path, "felis");
        let Err(err) = outcome else {
            panic!("expected an error, got {outcome:?}");
        };
        assert!(matches!(err, LoadError::Invalid { .. }), "got {err:?}");
        let diagnostics = err.diagnostics();
        assert!(diagnostics.has_errors());
        assert_eq!(
            diagnostics.errors().next().map(|d| d.kind),
            Some(DiagnosticKind::Parse),
        );
    }

    #[test]
    fn try_load_from_unknown_section_keeps_valid_sections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[font]\nfamily = \"JetBrains Mono\"\n\n[future_section]\nknob = true\n",
        )
        .unwrap();
        let cfg = EffectiveConfig::try_load_from(&path, "felis")
            .expect("unknown section must not fail the load");
        assert_eq!(cfg.font.family.as_deref(), Some("JetBrains Mono"));
    }

    /// The value-level twin of
    /// `try_load_from_unknown_section_keeps_valid_sections`: startup must
    /// load a document written by a newer felis, not fall to defaults.
    #[test]
    fn try_load_from_unknown_enum_token_keeps_valid_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            concat!(
                "[font]\nfamily = \"JetBrains Mono\"\n",
                "[cursor]\nblink = \"breathe\"\n",
                "[client.felis.window]\nbackdrop = \"vibrancy\"\n",
            ),
        )
        .unwrap();
        let cfg = EffectiveConfig::try_load_from(&path, "felis")
            .expect("an unknown enum token must not fail the load");
        assert_eq!(cfg.font.family.as_deref(), Some("JetBrains Mono"));
        assert_eq!(cfg.cursor.blink, CursorBlinkMode::Program);
        assert_eq!(cfg.window.backdrop, Backdrop::None);
    }

    #[test]
    fn load_falls_back_to_default_on_any_error() {
        let dir = tempfile::tempdir().unwrap();
        let parse_err_path = dir.path().join("broken.toml");
        std::fs::write(&parse_err_path, "this = is = not = toml").unwrap();
        let cfg = EffectiveConfig::try_load_from(&parse_err_path, "felis").unwrap_or_default();
        assert_eq!(cfg, EffectiveConfig::default());
    }

    #[test]
    fn clipboard_default_is_security_conscious() {
        let cfg = EffectiveConfig::default();
        assert!(cfg.clipboard.use_os_clipboard);
        assert_eq!(cfg.clipboard.osc_52, Osc52Policy::Mirror);
        assert!(!cfg.clipboard.osc_52.writes_to_system());
    }

    #[test]
    fn clipboard_section_parses_explicit_overrides() {
        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [clipboard]
            use_os_clipboard = false
            osc_52 = "system"
        "#,
        )
        .unwrap();
        assert!(!cfg.clipboard.use_os_clipboard);
        assert!(cfg.clipboard.osc_52.writes_to_system());
    }

    #[test]
    fn theme_palette_section_parses_named_slots() {
        let cfg: EffectiveConfig = toml::from_str(
            r##"
            [theme.palette]
            red = "#ff5555"
            bright_blue = "#5555ff"
        "##,
        )
        .unwrap();
        assert_eq!(cfg.theme.palette.red.as_deref(), Some("#ff5555"));
        assert_eq!(cfg.theme.palette.bright_blue.as_deref(), Some("#5555ff"));
        assert!(cfg.theme.palette.green.is_none());
    }

    #[test]
    fn palette_config_into_overrides_aligns_with_sgr_order() {
        // Field order is the contract that joins config and renderer.
        let palette = PaletteConfig {
            black: Some("#000000".into()),
            red: Some("#ff0000".into()),
            bright_white: Some("#ffffff".into()),
            ..Default::default()
        };
        let overrides = palette.into_overrides();
        assert_eq!(overrides.get(&0).map(String::as_str), Some("#000000"));
        assert_eq!(overrides.get(&1).map(String::as_str), Some("#ff0000"));
        assert_eq!(overrides.get(&15).map(String::as_str), Some("#ffffff"));
        assert_eq!(overrides.len(), 3);
    }

    #[test]
    fn palette_indexed_table_carries_extended_slots() {
        let cfg: EffectiveConfig = toml::from_str(
            r##"
            [theme.palette]
            red = "#ff5555"

            [theme.palette.indexed]
            16 = "#d08770"
            21 = "#eceff4"
        "##,
        )
        .unwrap();
        let overrides = cfg.theme.palette.into_overrides();
        assert_eq!(overrides.get(&1).map(String::as_str), Some("#ff5555"));
        assert_eq!(overrides.get(&16).map(String::as_str), Some("#d08770"));
        assert_eq!(overrides.get(&21).map(String::as_str), Some("#eceff4"));
    }

    #[test]
    fn palette_indexed_drops_base_and_out_of_range_keys() {
        let cfg: EffectiveConfig = toml::from_str(
            r##"
            [theme.palette.indexed]
            7 = "#111111"
            300 = "#222222"
            notanindex = "#333333"
            42 = "#abcdef"
        "##,
        )
        .unwrap();
        let overrides = cfg.theme.palette.into_overrides();
        assert_eq!(overrides.get(&42).map(String::as_str), Some("#abcdef"));
        assert_eq!(overrides.len(), 1);
    }

    #[test]
    fn unknown_palette_field_reported_so_typos_surface() {
        let (cfg, ignored) = parse_with_ignored(
            r##"
            [theme.palette]
            dark_red = "#ff0000"
            red = "#ff5555"
        "##,
            "felis",
        )
        .unwrap();
        assert_eq!(ignored, vec!["theme.palette.dark_red".to_owned()]);
        assert_eq!(cfg.theme.palette.red.as_deref(), Some("#ff5555"));
    }

    #[test]
    fn try_load_from_well_formed_file_returns_ok_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r##"
                [font]
                family = "Source Code Pro"
                size_px = 16.0

                [theme]
                foreground = "#ffffff"
            "##,
        )
        .unwrap();
        let cfg =
            EffectiveConfig::try_load_from(&path, "felis").expect("well-formed config must parse");
        assert_eq!(cfg.font.family.as_deref(), Some("Source Code Pro"));
        assert_eq!(cfg.font.size_px, Some(16.0));
        assert_eq!(cfg.theme.foreground.as_deref(), Some("#ffffff"));
        assert_eq!(cfg.theme.background, None);
    }

    #[test]
    fn resolve_reports_an_unknown_key_as_a_warning() {
        let document = ConfigDocument::parse("[window]\nprefix = \"[work]\"\n", None).unwrap();
        let (cfg, diagnostics) = document.resolve("felis");
        assert!(!diagnostics.has_errors(), "a typo must not fail the load");
        let warning = diagnostics.warnings().next().expect("one warning");
        assert_eq!(warning.kind, DiagnosticKind::UnknownKey);
        assert_eq!(warning.key.as_deref(), Some("window.prefix"));
        assert_eq!(cfg.window.title_prefix, None);
    }

    #[test]
    fn resolve_reports_a_type_error_as_an_error_and_falls_back() {
        let document = ConfigDocument::parse("[window]\ndecorations = \"yes\"\n", None).unwrap();
        let (cfg, diagnostics) = document.resolve("felis");
        assert!(diagnostics.has_errors());
        assert_eq!(
            diagnostics.errors().next().map(|d| d.kind),
            Some(DiagnosticKind::Parse),
        );
        assert_eq!(cfg, EffectiveConfig::default());
    }

    #[test]
    fn resolve_reports_a_malformed_keymap_entry_at_its_chord() {
        let document =
            ConfigDocument::parse("[keymap]\n\"ctrl+shift+q\" = { action = \"nope\" }\n", None)
                .unwrap();
        let (_, diagnostics) = document.resolve("felis");
        let warning = diagnostics.warnings().next().expect("one warning");
        assert_eq!(warning.kind, DiagnosticKind::Value);
        assert_eq!(warning.key.as_deref(), Some(r#"keymap."ctrl+shift+q""#));
    }

    #[test]
    fn resolve_reports_a_malformed_fallback_entry_at_its_index() {
        let document =
            ConfigDocument::parse("[font]\nfallback = [{ familly = \"Noto\" }]\n", None).unwrap();
        let (cfg, diagnostics) = document.resolve("felis");
        assert_eq!(cfg.font.fallback, Vec::<FontStyleConfig>::new());
        let warning = diagnostics.warnings().next().expect("one warning");
        assert_eq!(warning.kind, DiagnosticKind::Value);
        assert_eq!(warning.key.as_deref(), Some("font.fallback[0]"));
    }

    #[test]
    fn resolve_reports_an_overlay_value_at_the_overlay_path() {
        let document = ConfigDocument::parse(
            "[client.felis.font]\nsize_px = 200.0\nfallback = [{ familly = \"Noto\" }]\n",
            None,
        )
        .unwrap();
        let (_, diagnostics) = document.resolve("felis");
        let keys: Vec<_> = diagnostics
            .warnings()
            .filter_map(|d| d.key.as_deref())
            .collect();
        assert_eq!(
            keys,
            ["client.felis.font.size_px", "client.felis.font.fallback[0]"],
        );
    }

    #[test]
    fn resolve_reports_an_out_of_range_font_size_as_clamped() {
        let document = ConfigDocument::parse("[font]\nsize_px = 200.0\n", None).unwrap();
        let (cfg, diagnostics) = document.resolve("felis");
        assert_eq!(
            cfg.font.sanitized_font_size_logical_px(),
            Some(MAX_FONT_SIZE_LOGICAL_PX),
        );
        let warning = diagnostics.warnings().next().expect("one warning");
        assert_eq!(warning.kind, DiagnosticKind::Value);
        assert_eq!(warning.key.as_deref(), Some("font.size_px"));
    }

    #[test]
    fn a_font_size_written_as_a_toml_integer_equals_the_float_spelling() {
        let integer = ConfigDocument::parse("[font]\nsize_px = 14\n", None).unwrap();
        let float = ConfigDocument::parse("[font]\nsize_px = 14.0\n", None).unwrap();
        let (integer, integer_diagnostics) = integer.resolve("felis");
        let (float, float_diagnostics) = float.resolve("felis");
        assert!(integer_diagnostics.is_empty(), "{integer_diagnostics:?}");
        assert!(float_diagnostics.is_empty(), "{float_diagnostics:?}");
        assert_eq!(integer.font.size_px, Some(14.0));
        assert_eq!(integer, float);
    }

    #[test]
    fn resolve_reports_an_out_of_range_window_opacity_as_clamped() {
        let document = ConfigDocument::parse("[window]\nopacity = 2.5\n", None).unwrap();
        let (cfg, diagnostics) = document.resolve("felis");
        let clamped = cfg.window.clamped_opacity();
        assert_eq!(clamped, 1.0);
        let warning = diagnostics.warnings().next().expect("one warning");
        assert_eq!(warning.kind, DiagnosticKind::Value);
        assert_eq!(warning.key.as_deref(), Some("window.opacity"));
        assert!(
            warning.message.contains(&format!("clamped to {clamped}")),
            "the warning names the clamped value: {}",
            warning.message,
        );
    }

    #[test]
    fn resolve_reports_a_negative_scroll_multiplier_as_clamped() {
        let document = ConfigDocument::parse("[mouse]\nscroll_multiplier = -1\n", None).unwrap();
        let (cfg, diagnostics) = document.resolve("felis");
        let clamped = cfg.mouse.clamped_scroll_multiplier();
        assert!((clamped - 0.1).abs() < f64::EPSILON);
        let warning = diagnostics.warnings().next().expect("one warning");
        assert_eq!(warning.kind, DiagnosticKind::Value);
        assert_eq!(warning.key.as_deref(), Some("mouse.scroll_multiplier"));
        assert!(
            warning.message.contains(&format!("clamped to {clamped}")),
            "the warning names the clamped value: {}",
            warning.message,
        );
    }

    #[test]
    fn a_scroll_multiplier_one_ulp_below_the_floor_is_reported_as_clamped() {
        let document =
            ConfigDocument::parse("[mouse]\nscroll_multiplier = 0.09999999999999999\n", None)
                .unwrap();
        let (cfg, diagnostics) = document.resolve("felis");
        let clamped = cfg.mouse.clamped_scroll_multiplier();
        let warning = diagnostics.warnings().next().expect("one warning");
        assert_eq!(warning.kind, DiagnosticKind::Value);
        assert_eq!(warning.key.as_deref(), Some("mouse.scroll_multiplier"));
        assert!(
            warning.message.contains(&format!("clamped to {clamped}")),
            "the warning names the clamped value: {}",
            warning.message,
        );
    }

    #[test]
    fn a_non_finite_scroll_multiplier_falls_back_to_the_default_and_warns() {
        let document = ConfigDocument::parse("[mouse]\nscroll_multiplier = nan\n", None).unwrap();
        let (cfg, diagnostics) = document.resolve("felis");
        assert!(
            (cfg.mouse.clamped_scroll_multiplier() - DEFAULT_SCROLL_MULTIPLIER).abs()
                < f64::EPSILON,
        );
        let warning = diagnostics.warnings().next().expect("one warning");
        assert_eq!(warning.key.as_deref(), Some("mouse.scroll_multiplier"));
        assert!(
            warning
                .message
                .contains(&format!("using {DEFAULT_SCROLL_MULTIPLIER}")),
            "the warning names the fallback value: {}",
            warning.message,
        );
    }

    #[test]
    fn in_band_opacity_and_scroll_multiplier_produce_no_diagnostic() {
        let document = ConfigDocument::parse(
            "[window]\nopacity = 0.85\n\n[mouse]\nscroll_multiplier = 5\n",
            None,
        )
        .unwrap();
        let (_, diagnostics) = document.resolve("felis");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn a_missing_shader_file_is_a_warning_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[shader]\npost = { file = \"fx/glow.wgsl\" }\n").unwrap();
        let cfg = EffectiveConfig::try_load_from(&path, "felis")
            .expect("a missing shader must not fail the load");
        assert_eq!(
            cfg.post_shader_choice(),
            PostShaderChoice::File(dir.path().join("fx/glow.wgsl")),
        );

        let (_, diagnostics) = EffectiveConfig::diagnose(&path, "felis");
        assert!(!diagnostics.has_errors());
        let warning = diagnostics.warnings().next().expect("one warning");
        assert_eq!(warning.kind, DiagnosticKind::MissingFile);
        assert_eq!(warning.key.as_deref(), Some("shader.post.file"));
    }

    #[test]
    fn a_shader_file_beside_the_config_resolves_and_is_silent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("fx")).unwrap();
        std::fs::write(dir.path().join("fx/glow.wgsl"), "// wgsl").unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[shader]\npost = { file = \"fx/glow.wgsl\" }\n").unwrap();

        let (cfg, diagnostics) = EffectiveConfig::diagnose(&path, "felis");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(
            cfg.post_shader_choice(),
            PostShaderChoice::File(dir.path().join("fx/glow.wgsl")),
        );
    }

    #[test]
    fn an_absolute_or_tilde_shader_path_ignores_the_config_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // Not a `/opt/...` literal: that is a relative path on Windows.
        let elsewhere = dir.path().join("fx").join("glow.wgsl");
        let toml = format!(
            "[shader]\npost = {{ file = \"{}\" }}\n",
            elsewhere.display().to_string().replace('\\', "\\\\")
        );
        std::fs::write(&path, toml).unwrap();
        let (cfg, _) = EffectiveConfig::diagnose(&path, "felis");
        assert_eq!(cfg.post_shader_choice(), PostShaderChoice::File(elsewhere));
    }

    fn pipe_target(cfg: &EffectiveConfig, chord: &str) -> crate::action::PipeTarget {
        let compiled: BTreeMap<Chord, BindingValue> = cfg
            .keymap
            .compile(cfg.source_dir.as_deref())
            .into_iter()
            .collect();
        let binding = compiled
            .get(&chord.parse::<Chord>().unwrap())
            .expect("the chord is bound")
            .clone();
        match binding.into_action() {
            Some(Action::PipeRegion { target, .. }) => target,
            other => panic!("expected a pipe binding, got {other:?}"),
        }
    }

    #[test]
    fn a_pipe_file_sink_resolves_against_the_config_directory() {
        use crate::action::PipeTarget;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[keymap]\n\"ctrl+shift+o\" = { kind = \"pipe\", source = \"scrollback\", \
             target = { file = \"dumps/region.txt\" } }\n",
        )
        .unwrap();

        let cfg = EffectiveConfig::try_load_from(&path, "felis").unwrap();
        assert_eq!(
            pipe_target(&cfg, "ctrl+shift+o"),
            PipeTarget::File(Some(dir.path().join("dumps/region.txt"))),
        );
    }

    #[test]
    fn an_absolute_or_tilde_pipe_file_sink_ignores_the_config_directory() {
        use crate::action::PipeTarget;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // Not a `/opt/...` literal: that is a relative path on Windows.
        let elsewhere = dir.path().join("dumps").join("region.txt");
        let toml = format!(
            "[keymap]\n\"ctrl+shift+o\" = {{ kind = \"pipe\", source = \"scrollback\", \
             target = {{ file = \"{}\" }} }}\n\"ctrl+shift+p\" = {{ kind = \"pipe\", \
             source = \"scrollback\", target = {{ file = \"~/region.txt\" }} }}\n",
            elsewhere.display().to_string().replace('\\', "\\\\"),
        );
        std::fs::write(&path, toml).unwrap();

        let cfg = EffectiveConfig::try_load_from(&path, "felis").unwrap();
        assert_eq!(
            pipe_target(&cfg, "ctrl+shift+o"),
            PipeTarget::File(Some(elsewhere)),
        );
        let home = directories::BaseDirs::new().expect("a home directory");
        assert_eq!(
            pipe_target(&cfg, "ctrl+shift+p"),
            PipeTarget::File(Some(home.home_dir().join("region.txt"))),
        );
    }

    #[test]
    fn a_tilde_expands_only_when_a_separator_follows_it() {
        let home = directories::BaseDirs::new().expect("a home directory");
        let base = Path::new("/cfg");
        assert_eq!(
            resolve_path(Path::new("~/"), Some(base)),
            home.home_dir().to_path_buf()
        );
        assert_eq!(
            resolve_path(Path::new("~/x"), Some(base)),
            home.home_dir().join("x")
        );
        assert_eq!(resolve_path(Path::new("~"), Some(base)), base.join("~"));
        assert_eq!(resolve_path(Path::new("~x"), Some(base)), base.join("~x"));
    }

    #[test]
    fn a_pipe_file_sink_parsed_from_text_stays_relative_to_the_working_directory() {
        use crate::action::PipeTarget;

        let cfg: EffectiveConfig = toml::from_str(
            r#"
            [keymap]
            "ctrl+shift+o" = { kind = "pipe", source = "scrollback", target = { file = "region.txt" } }
        "#,
        )
        .unwrap();
        assert_eq!(cfg.source_dir, None);
        assert_eq!(
            pipe_target(&cfg, "ctrl+shift+o"),
            PipeTarget::File(Some(PathBuf::from("region.txt"))),
        );
    }

    #[test]
    fn diagnose_reports_an_unreadable_file_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::create_dir(&path).unwrap();
        let (cfg, diagnostics) = EffectiveConfig::diagnose(&path, "felis");
        assert_eq!(cfg, EffectiveConfig::default());
        assert_eq!(
            diagnostics.errors().next().map(|d| d.kind),
            Some(DiagnosticKind::Io),
        );
    }

    #[test]
    fn a_read_failure_carries_its_own_diagnostic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::create_dir(&path).unwrap();
        let err = EffectiveConfig::try_load_from(&path, "felis").unwrap_err();
        assert!(matches!(err, LoadError::Read { .. }), "got {err:?}");
        assert_eq!(
            err.diagnostics().errors().next().map(|d| d.kind),
            Some(DiagnosticKind::Io),
        );
    }

    #[test]
    fn client_overlay_overrides_base_field_keeps_siblings() {
        let (cfg, ignored) = parse_with_ignored(
            r#"
            [font]
            family = "JetBrains Mono"
            size_px = 14.0

            [client.felis.font]
            size_px = 16.0
        "#,
            "felis",
        )
        .unwrap();
        assert_eq!(ignored, Vec::<String>::new());
        assert_eq!(cfg.font.size_px, Some(16.0));
        assert_eq!(cfg.font.family.as_deref(), Some("JetBrains Mono"));
    }

    #[test]
    fn client_overlay_for_other_client_is_inert_and_unwarned() {
        let (cfg, ignored) = parse_with_ignored(
            r"
            [font]
            size_px = 14.0

            [client.felis-tui]
            knob_we_never_heard_of = true

            [client.felis-tui.font]
            size_px = 99.0
        ",
            "felis",
        )
        .unwrap();
        assert!(
            ignored.is_empty(),
            "foreign overlay must not warn: {ignored:?}"
        );
        assert_eq!(cfg.font.size_px, Some(14.0));
        assert!(cfg.client.contains_key("felis-tui"));
    }

    #[test]
    fn client_overlay_arrays_replace_not_merge() {
        let (cfg, _) = parse_with_ignored(
            r#"
            [font]
            fallback = [
                { family = "Noto Sans CJK JP" },
                { family = "Noto Color Emoji" },
            ]
            features = ["calt", "liga"]

            [client.felis.font]
            features = ["calt"]
        "#,
            "felis",
        )
        .unwrap();
        assert_eq!(cfg.font.features, vec!["calt".to_owned()]);
        assert_eq!(cfg.font.fallback.len(), 2);
    }

    #[test]
    fn client_overlay_keymap_merges_per_chord() {
        let (cfg, _) = parse_with_ignored(
            r#"
            [keymap]
            "ctrl+shift+r" = { kind = "reload" }
            "ctrl+shift+f" = { kind = "open_scrollback_search" }

            [client.felis.keymap]
            "ctrl+shift+r" = { kind = "unbind" }
            "ctrl+shift+n" = { kind = "new_session" }
        "#,
            "felis",
        )
        .unwrap();
        let compiled: BTreeMap<Chord, BindingValue> =
            cfg.keymap.compile(None).into_iter().collect();
        assert_eq!(compiled.len(), 3);
        assert_eq!(
            compiled.get(&"ctrl+shift+r".parse().unwrap()),
            Some(&BindingValue::Unbind),
        );
        assert_eq!(
            compiled.get(&"ctrl+shift+f".parse().unwrap()),
            Some(&BindingValue::OpenScrollbackSearch),
        );
        assert_eq!(
            compiled.get(&"ctrl+shift+n".parse().unwrap()),
            Some(&BindingValue::NewSession),
        );
    }

    #[test]
    fn client_overlay_typo_reported_at_written_path() {
        let (cfg, ignored) = parse_with_ignored(
            r"
            [client.felis.font]
            szie = 16.0
            size_px = 18.0
        ",
            "felis",
        )
        .unwrap();
        assert_eq!(ignored, vec!["client.felis.font.szie".to_owned()]);
        assert_eq!(cfg.font.size_px, Some(18.0));
    }

    #[test]
    fn client_overlay_non_table_is_reported_and_skipped() {
        let (cfg, diagnostics) = ConfigDocument::parse(
            r#"
            client = { felis = "oops" }

            [font]
            size_px = 14.0
        "#,
            None,
        )
        .unwrap()
        .resolve("felis");
        assert_eq!(diagnostics.unknown_keys(), Vec::<&str>::new());
        let reported: Vec<_> = diagnostics
            .warnings()
            .filter(|d| d.kind == DiagnosticKind::Value)
            .filter_map(|d| d.key.as_deref())
            .collect();
        assert_eq!(reported, ["client.felis"]);
        assert_eq!(cfg.font.size_px, Some(14.0));
    }

    #[test]
    fn client_overlay_nested_client_is_reported_and_stripped() {
        let (cfg, diagnostics) = ConfigDocument::parse(
            r"
            [client.felis]
            font = { size_px = 16.0 }
            client = { felis = { font = { size_px = 99.0 } } }
        ",
            None,
        )
        .unwrap()
        .resolve("felis");
        assert_eq!(diagnostics.unknown_keys(), Vec::<&str>::new());
        let reported: Vec<_> = diagnostics
            .warnings()
            .filter(|d| d.kind == DiagnosticKind::Value)
            .filter_map(|d| d.key.as_deref())
            .collect();
        assert_eq!(reported, ["client.felis.client"]);
        assert_eq!(cfg.font.size_px, Some(16.0));
    }

    #[test]
    fn client_overlay_absent_id_leaves_base_untouched() {
        let (cfg, ignored) = parse_with_ignored(
            r"
            [font]
            size_px = 14.0
        ",
            "no-such-client",
        )
        .unwrap();
        assert_eq!(ignored, Vec::<String>::new());
        assert_eq!(cfg.font.size_px, Some(14.0));
    }

    #[test]
    fn compose_window_title_pass_through_when_no_prefix() {
        assert_eq!(compose_window_title(None, "zsh: ~/work"), "zsh: ~/work");
        assert_eq!(compose_window_title(Some(""), "zsh"), "zsh");
        assert_eq!(compose_window_title(Some("   "), "zsh"), "zsh");
    }

    #[test]
    fn compose_window_title_prepends_prefix_with_single_space() {
        assert_eq!(
            compose_window_title(Some("[work]"), "zsh: ~/work"),
            "[work] zsh: ~/work",
        );
        assert_eq!(compose_window_title(Some("📦"), "vim"), "📦 vim");
    }

    #[test]
    fn compose_window_title_prefix_only_when_shell_title_empty() {
        assert_eq!(compose_window_title(Some("[work]"), ""), "[work]");
    }

    fn font_size(size: Option<f32>) -> FontConfig {
        FontConfig {
            size_px: size,
            ..Default::default()
        }
    }

    #[test]
    fn sanitized_font_size_returns_none_for_unset() {
        assert_eq!(font_size(None).sanitized_font_size_logical_px(), None);
    }

    /// What every clamped knob owes its raw value: unchanged inside the
    /// band, pinned to the nearer edge outside it.
    fn banded<T: PartialOrd>(raw: T, min: T, max: T) -> T {
        if raw < min {
            min
        } else if raw > max {
            max
        } else {
            raw
        }
    }

    proptest::proptest! {
        #[test]
        fn sanitized_font_size_is_its_raw_value_banded(raw in 1e-6_f32..=1e6_f32) {
            let out = font_size(Some(raw)).sanitized_font_size_logical_px().expect("positive finite must yield Some");
            proptest::prop_assert_eq!(
                out,
                banded(raw, MIN_FONT_SIZE_LOGICAL_PX, MAX_FONT_SIZE_LOGICAL_PX),
            );
        }

        #[test]
        fn a_font_size_step_lands_on_the_banded_sum(
            current in 1e-6_f32..=1e6_f32,
            delta in -1e6_f32..=1e6_f32,
        ) {
            proptest::prop_assert_eq!(
                FontConfig::clamped_font_size(current, delta),
                banded(current + delta, MIN_FONT_SIZE_LOGICAL_PX, MAX_FONT_SIZE_LOGICAL_PX),
            );
        }

        #[test]
        fn clamped_opacity_is_its_raw_value_banded(opacity in -1e3_f32..=1e3_f32) {
            let cfg = WindowConfig { opacity, ..WindowConfig::default() };
            proptest::prop_assert_eq!(
                cfg.clamped_opacity(),
                banded(opacity, MIN_OPACITY, MAX_OPACITY),
            );
        }

        #[test]
        fn clamped_scroll_multiplier_is_its_raw_value_banded(
            scroll_multiplier in -1e4_f64..=1e4_f64,
        ) {
            let cfg = MouseConfig { scroll_multiplier };
            proptest::prop_assert_eq!(
                cfg.clamped_scroll_multiplier(),
                banded(scroll_multiplier, MIN_SCROLL_MULTIPLIER, MAX_SCROLL_MULTIPLIER),
            );
        }

        #[test]
        fn sanitized_font_size_drops_non_positive_or_non_finite_to_none(
            raw in proptest::prop_oneof![
                proptest::strategy::Just(f32::NAN),
                proptest::strategy::Just(f32::INFINITY),
                proptest::strategy::Just(f32::NEG_INFINITY),
                (-1e6_f32..=0.0_f32),
            ],
        ) {
            proptest::prop_assert_eq!(font_size(Some(raw)).sanitized_font_size_logical_px(), None);
        }
    }
}

#[cfg(all(test, feature = "schema"))]
mod schema {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::EffectiveConfig;

    const SCHEMA_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/felis-config.schema.json");

    fn rendered() -> String {
        let schema = schemars::schema_for!(EffectiveConfig);
        let mut json = serde_json::to_string_pretty(&schema).unwrap();
        json.push('\n');
        json
    }

    #[test]
    fn config_schema_is_up_to_date() {
        let want = rendered();
        if std::env::var_os("UPDATE_SCHEMA").is_some() {
            std::fs::write(SCHEMA_PATH, &want).unwrap();
            return;
        }
        let have = std::fs::read_to_string(SCHEMA_PATH).expect(
            "crates/felis-client-core/felis-config.schema.json missing; run `UPDATE_SCHEMA=1 cargo test -p felis-client-core --features schema`",
        );
        assert_eq!(
            have, want,
            "config JSON schema is stale; regenerate with \
             `UPDATE_SCHEMA=1 cargo test -p felis-client-core --features schema`",
        );
    }

    /// The runtime validates this client's overlay and carries every other
    /// client's section as raw TOML; an editor must draw the same line.
    #[test]
    fn the_client_map_validates_only_this_client_s_overlay() {
        let schema = serde_json::to_value(schemars::schema_for!(EffectiveConfig)).unwrap();
        let client = &schema["properties"]["client"];
        assert_eq!(
            client["properties"],
            serde_json::json!({ "felis": { "$ref": "#" } })
        );
        assert_eq!(client["additionalProperties"], serde_json::json!(true));
    }

    /// Frozen for 1.0: an unrecognized section key warns at load time, so
    /// the published schema must not turn one into a validation error.
    /// Nested typed values inside a section may still close by shape.
    #[test]
    fn neither_the_root_nor_a_config_section_is_closed() {
        let schema = serde_json::to_value(schemars::schema_for!(EffectiveConfig)).unwrap();
        assert!(schema.get("additionalProperties").is_none(), "{schema}");
        for (name, section) in schema["$defs"].as_object().unwrap() {
            if !name.ends_with("Config") {
                continue;
            }
            assert!(
                section.get("additionalProperties") != Some(&serde_json::json!(false)),
                "{name} is closed",
            );
        }
    }
}

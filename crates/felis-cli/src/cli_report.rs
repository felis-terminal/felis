//! The facts `felis doctor report` adds to the checklist: what a
//! maintainer needs to set up the reporter's environment, redacted
//! before either rendering sees it (docs/reference/cli.md "Doctor
//! report").

use std::fmt::Write as _;
use std::path::Path;

use felis_client_core::config::{GUI_CLIENT_ID, KeymapConfig};
use felis_client_core::doctor::{FontsProbe, ProbeReport};
use felis_client_core::{BindingValue, ConfigSource, EffectiveConfig, PipeSink};
use felis_protocol::BuildIdentity;

use crate::cli_output::{
    BuildsObject, ConfigObject, DisplayObject, DoctorReportResult, EnvironmentObject, FontsObject,
    GpuObject, LocaleObject, LogObject, OsObject, ShellObject,
};
use crate::cli_version::SELF_VERSION_LINE;

/// What the checklist run already learned, handed over rather than
/// asked again.
pub(crate) struct Inputs<'a> {
    pub(crate) probe: &'a Result<ProbeReport, String>,
    pub(crate) fonts_note: Option<&'a str>,
    pub(crate) daemon: Option<&'a BuildIdentity>,
    pub(crate) config_source: &'a ConfigSource,
}

/// Only the named variables are ever read; nothing walks the
/// environment.
pub(crate) fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Unredacted: [`HomeCollapse::redact`] runs over the assembled result.
pub(crate) fn environment(
    inputs: &Inputs<'_>,
    var: &dyn Fn(&str) -> Option<String>,
) -> EnvironmentObject {
    let probe = inputs.probe.as_ref().ok();
    EnvironmentObject {
        builds: BuildsObject {
            cli: SELF_VERSION_LINE.to_owned(),
            client: probe.map(|report| report.client_version.clone()),
            daemon: inputs.daemon.map(BuildIdentity::human),
        },
        binary: std::env::current_exe()
            .ok()
            .map(|path| path.to_string_lossy().into_owned()),
        os: os_object(),
        display: DisplayObject {
            session_type: var("XDG_SESSION_TYPE"),
            desktop: var("XDG_CURRENT_DESKTOP"),
            wayland: var("WAYLAND_DISPLAY").is_some(),
            x11: var("DISPLAY").is_some(),
        },
        gpu: probe.map(|report| GpuObject {
            available: report.gpu.available,
            name: report.gpu.name.clone(),
            backend: report.gpu.backend.clone(),
            device_type: report.gpu.device_type.clone(),
            driver: report.gpu.driver.clone(),
            driver_info: report.gpu.driver_info.clone(),
        }),
        locale: LocaleObject {
            lang: var("LANG"),
            lc_all: var("LC_ALL"),
            lc_ctype: var("LC_CTYPE"),
        },
        shell: ShellObject {
            inside_felis: var("FELIS_SESSION_ID").is_some(),
            term: var("TERM"),
            term_program: var("TERM_PROGRAM"),
            term_program_version: var("TERM_PROGRAM_VERSION"),
            colorterm: var("COLORTERM"),
            ssh: var("SSH_CONNECTION").is_some(),
        },
        fonts: fonts_object(inputs.probe, inputs.fonts_note),
        config: config_object(inputs.config_source),
        logs: log_objects(),
    }
}

fn fonts_object(probe: &Result<ProbeReport, String>, note: Option<&str>) -> FontsObject {
    let unavailable = |why: String| FontsObject {
        regular: None,
        bold: None,
        italic: None,
        bold_italic: None,
        fallbacks: Vec::new(),
        unavailable: Some(why),
    };
    match probe {
        Err(why) => unavailable(why.clone()),
        Ok(report) => match &report.fonts {
            Some(FontsProbe::Resolved {
                regular,
                bold,
                italic,
                bold_italic,
                fallbacks,
            }) => FontsObject {
                regular: Some(regular.clone()),
                bold: Some(bold.clone()),
                italic: Some(italic.clone()),
                bold_italic: Some(bold_italic.clone()),
                fallbacks: fallbacks.clone(),
                unavailable: None,
            },
            Some(FontsProbe::Failed { error }) => unavailable(format!("no font resolved: {error}")),
            None => unavailable(
                note.unwrap_or("the frontend reported no font stack")
                    .to_owned(),
            ),
        },
    }
}

fn config_object(source: &ConfigSource) -> ConfigObject {
    let path = source.path();
    let (config, diagnostics) = EffectiveConfig::diagnose_source(source, GUI_CLIENT_ID);
    let state = if diagnostics.errors().next().is_some() {
        "invalid"
    } else if path.as_deref().is_some_and(Path::is_file) {
        "applied"
    } else {
        "absent"
    };
    // Past an error `diagnose` hands back the defaults, whose diff is
    // empty; nothing of the broken file is shown.
    let diff = config_diff(&config);
    ConfigObject {
        path: path.map(|path| path.to_string_lossy().into_owned()),
        state: state.to_owned(),
        diff: serde_json::to_value(&diff).unwrap_or_default(),
    }
}

fn log_objects() -> Vec<LogObject> {
    let Some(dir) = felis_transport::logging::log_dir() else {
        return Vec::new();
    };
    ["daemon.log", "client.log"]
        .into_iter()
        .map(|name| {
            let path = dir.join(name);
            LogObject {
                name: name.to_owned(),
                path: path.to_string_lossy().into_owned(),
                present: path.is_file(),
            }
        })
        .collect()
}

fn os_object() -> OsObject {
    let (release, kernel) = os_versions();
    OsObject {
        family: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        release,
        kernel,
    }
}

#[cfg(target_os = "linux")]
fn os_versions() -> (Option<String>, Option<String>) {
    let release = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| os_release_pretty_name(&text));
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty());
    (release, kernel)
}

#[cfg(target_os = "macos")]
fn os_versions() -> (Option<String>, Option<String>) {
    let run = |program: &str, args: &[&str]| {
        std::process::Command::new(program)
            .args(args)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .filter(|text| !text.is_empty())
    };
    (
        run("sw_vers", &["-productVersion"]).map(|version| format!("macOS {version}")),
        run("uname", &["-r"]),
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const fn os_versions() -> (Option<String>, Option<String>) {
    (None, None)
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn os_release_pretty_name(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("PRETTY_NAME="))
        .map(|value| value.trim().trim_matches(['"', '\'']).to_owned())
        .filter(|value| !value.is_empty())
}

/// GitHub-flavored Markdown, so the report pastes into an issue as is.
/// `daemon_detail` is the `daemon` row's prose, shown when no identity
/// came back.
pub(crate) fn markdown(report: &DoctorReportResult, daemon_detail: &str) -> String {
    let env = &report.environment;
    let mut out = String::new();
    let unset = || "unset".to_owned();
    let _ = writeln!(
        out,
        "<!-- felis doctor report: review before posting. Paths under your home directory are \
         shown as ~, and keybinding commands and texts as <redacted>. -->\n"
    );

    let _ = writeln!(out, "### felis\n");
    let _ = writeln!(out, "| component | build |\n| --- | --- |");
    let _ = writeln!(out, "| cli | {} |", cell(&env.builds.cli));
    let _ = writeln!(
        out,
        "| client | {} |",
        cell(
            env.builds
                .client
                .as_deref()
                .unwrap_or("not probed (see the gpu check)")
        )
    );
    let _ = writeln!(
        out,
        "| daemon | {} |",
        cell(env.builds.daemon.as_deref().unwrap_or(daemon_detail))
    );
    let _ = writeln!(
        out,
        "| binary | {} |\n",
        cell(env.binary.as_deref().unwrap_or("unknown"))
    );

    let _ = writeln!(out, "### System\n");
    let os = &env.os;
    let mut os_line = format!("{} {}", os.family, os.arch);
    for part in [&os.release, &os.kernel].into_iter().flatten() {
        let _ = write!(os_line, ", {part}");
    }
    let _ = writeln!(out, "- os: {os_line}");
    let display = &env.display;
    let _ = writeln!(
        out,
        "- display: XDG_SESSION_TYPE={}, XDG_CURRENT_DESKTOP={}, WAYLAND_DISPLAY {}, DISPLAY {}",
        display.session_type.clone().unwrap_or_else(unset),
        display.desktop.clone().unwrap_or_else(unset),
        set_or_unset(display.wayland),
        set_or_unset(display.x11),
    );
    let _ = writeln!(out, "- gpu: {}", gpu_line(env.gpu.as_ref()));
    let locale = &env.locale;
    let _ = writeln!(
        out,
        "- locale: LANG={}, LC_ALL={}, LC_CTYPE={}",
        locale.lang.clone().unwrap_or_else(unset),
        locale.lc_all.clone().unwrap_or_else(unset),
        locale.lc_ctype.clone().unwrap_or_else(unset),
    );
    let shell = &env.shell;
    let identity = format!(
        "TERM={}, TERM_PROGRAM={}, TERM_PROGRAM_VERSION={}, COLORTERM={}",
        shell.term.clone().unwrap_or_else(unset),
        shell.term_program.clone().unwrap_or_else(unset),
        shell.term_program_version.clone().unwrap_or_else(unset),
        shell.colorterm.clone().unwrap_or_else(unset),
    );
    if shell.inside_felis {
        let _ = writeln!(out, "- shell: inside a felis session; {identity}");
    } else {
        let _ = writeln!(
            out,
            "- shell: not run inside a felis session, so the affected session's terminal \
             identity is unknown (rerun from the affected window); this shell has {identity}"
        );
    }
    let _ = writeln!(out, "- ssh: {}\n", if shell.ssh { "yes" } else { "no" });

    let _ = writeln!(out, "### Fonts\n");
    let fonts = &env.fonts;
    if let Some(why) = &fonts.unavailable {
        let _ = writeln!(out, "- unavailable: {why}\n");
    } else {
        let name = |face: &Option<String>| face.clone().unwrap_or_default();
        let _ = writeln!(out, "- regular: {}", name(&fonts.regular));
        let _ = writeln!(out, "- bold: {}", name(&fonts.bold));
        let _ = writeln!(out, "- italic: {}", name(&fonts.italic));
        let _ = writeln!(out, "- bold italic: {}", name(&fonts.bold_italic));
        let fallbacks = if fonts.fallbacks.is_empty() {
            "none".to_owned()
        } else {
            fonts.fallbacks.join(", ")
        };
        let _ = writeln!(out, "- fallbacks: {fallbacks}\n");
    }

    let config = &env.config;
    let path = config.path.as_deref().unwrap_or("no config directory");
    let _ = writeln!(out, "### Config\n");
    match config.state.as_str() {
        "absent" => {
            let _ = writeln!(out, "No file at {}; the defaults apply.\n", code_span(path));
        }
        "invalid" => {
            let _ = writeln!(
                out,
                "{} has errors, so the defaults apply (see the config check).\n",
                code_span(path)
            );
        }
        _ if config
            .diff
            .as_object()
            .is_none_or(serde_json::Map::is_empty) =>
        {
            let _ = writeln!(
                out,
                "{} sets nothing that differs from the defaults.\n",
                code_span(path)
            );
        }
        _ => {
            let text = toml::Value::try_from(&config.diff)
                .ok()
                .and_then(|value| toml::to_string_pretty(&value).ok())
                .unwrap_or_default();
            let fence = "`".repeat(longest_backtick_run(&text).max(2) + 1);
            let _ = writeln!(
                out,
                "<details><summary>Keys in <code>{}</code> that differ from the \
                 defaults</summary>\n\n{fence}toml\n{}\n{fence}\n\n</details>\n",
                html_escape(path),
                text.trim_end()
            );
        }
    }

    let _ = writeln!(out, "### Checks\n");
    let _ = writeln!(out, "| status | check | detail |\n| --- | --- | --- |");
    for check in &report.checks {
        let _ = writeln!(
            out,
            "| {} | {} | {} |",
            check.status,
            check.check,
            cell(&check.detail)
        );
    }

    let _ = writeln!(out, "\n### Logs\n");
    if env.logs.is_empty() {
        let _ = writeln!(out, "- no log directory (felis found no home directory)");
    }
    for log in &env.logs {
        let _ = writeln!(
            out,
            "- {}: {}{}",
            log.name,
            code_span(&log.path),
            if log.present { "" } else { " (missing)" }
        );
    }
    out
}

const fn set_or_unset(set: bool) -> &'static str {
    if set { "set" } else { "unset" }
}

fn gpu_line(gpu: Option<&GpuObject>) -> String {
    let Some(gpu) = gpu else {
        return "not probed (see the gpu check)".to_owned();
    };
    if !gpu.available {
        return "no adapter".to_owned();
    }
    let mut parts = vec![
        gpu.backend
            .clone()
            .unwrap_or_else(|| "unknown backend".to_owned()),
        gpu.device_type
            .clone()
            .unwrap_or_else(|| "unknown type".to_owned()),
    ];
    let driver = [&gpu.driver, &gpu.driver_info]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    if !driver.is_empty() {
        parts.push(format!("driver {driver}"));
    }
    format!(
        "{} ({})",
        gpu.name.as_deref().unwrap_or("adapter"),
        parts.join(", ")
    )
}

/// A code span whose delimiter outruns any backtick run inside it.
fn code_span(text: &str) -> String {
    let ticks = "`".repeat(longest_backtick_run(text) + 1);
    let pad = if text.starts_with('`') || text.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{ticks}{pad}{text}{pad}{ticks}")
}

fn longest_backtick_run(text: &str) -> usize {
    text.split(|c| c != '`').map(str::len).max().unwrap_or(0)
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A `|` or a line break inside a cell would end it. GFM strips the
/// escape off `\|` before reading the cell inline, so the backslashes
/// already before a pipe are doubled, or they would vanish from the
/// text. `&#124;` would print as is inside a code span.
fn cell(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '|' => {
                let run = out.len() - out.trim_end_matches('\\').len();
                out.push_str(&"\\".repeat(run));
                out.push_str("\\|");
            }
            '\n' | '\r' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

const REDACTED: &str = "<redacted>";

/// The home directory at a path boundary becomes `~`, so a username in
/// it does not reach a public issue.
pub(crate) struct HomeCollapse {
    /// Longest first, so a home nested in another collapses whole.
    homes: Vec<String>,
}

impl HomeCollapse {
    /// `$HOME` and the platform's own answer, which differ where the
    /// platform does not read `HOME` (an unset one on Unix, the profile
    /// folder on Windows): the config and log paths come from the latter.
    pub(crate) fn discover() -> Self {
        let platform = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
        let env = std::env::var_os("HOME").map(std::path::PathBuf::from);
        Self::new([platform.as_deref(), env.as_deref()].into_iter().flatten())
    }

    pub(crate) fn new<'a>(homes: impl IntoIterator<Item = &'a Path>) -> Self {
        let mut homes: Vec<String> = homes
            .into_iter()
            .map(|path| {
                path.to_string_lossy()
                    .trim_end_matches(['/', '\\'])
                    .to_owned()
            })
            // A root or empty home would rewrite every absolute path.
            .filter(|home| home.len() > 1)
            .collect();
        homes.sort_by_key(|home| std::cmp::Reverse(home.len()));
        homes.dedup();
        Self { homes }
    }

    pub(crate) fn apply(&self, text: &str) -> String {
        self.homes
            .iter()
            .fold(text.to_owned(), |text, home| collapse(&text, home))
    }

    /// Every string in the report, the field names aside, so a field
    /// added later is covered without naming it here.
    pub(crate) fn redact(&self, report: DoctorReportResult) -> Result<DoctorReportResult, String> {
        let mut value = serde_json::to_value(report).map_err(|err| err.to_string())?;
        self.redact_value(&mut value);
        serde_json::from_value(value).map_err(|err| err.to_string())
    }

    fn redact_value(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) => *text = self.apply(text),
            serde_json::Value::Array(items) => {
                for item in items.iter_mut() {
                    self.redact_value(item);
                }
            }
            serde_json::Value::Object(fields) => {
                let entries = std::mem::take(fields);
                for (key, mut item) in entries {
                    self.redact_value(&mut item);
                    fields.insert(self.apply(&key), item);
                }
            }
            _ => {}
        }
    }
}

fn collapse(text: &str, home: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(home) {
        let after = &rest[at + home.len()..];
        // `/home/al` must not collapse inside `/home/alice`; anything
        // that cannot continue a file name (`/`, `:`, a space) ends it.
        let boundary = !after.starts_with(|c: char| c.is_alphanumeric() || "_-.".contains(c));
        out.push_str(&rest[..at]);
        out.push_str(if boundary { "~" } else { home });
        rest = after;
    }
    out.push_str(rest);
    out
}

/// The keys of `config` whose values differ from the defaults, nested as
/// a TOML table in `config.toml`'s layout; a redacted binding does not
/// reproduce the user's. `[client.*]` is left out: it carries every other
/// client's unvalidated overlay, and the `felis` one is already merged
/// into `config`.
pub(crate) fn config_diff(config: &EffectiveConfig) -> toml::Table {
    let (Ok(toml::Value::Table(mut effective)), Ok(toml::Value::Table(defaults))) = (
        toml::Value::try_from(config),
        toml::Value::try_from(EffectiveConfig::default()),
    ) else {
        return toml::Table::new();
    };
    effective.remove("client");
    let mut diff = diff_tables(&effective, &defaults);
    if let Some(toml::Value::Table(keymap)) = diff.get_mut("keymap") {
        // A binding the loader drops has no known shape to project, and
        // none of it takes effect. Each entry goes through the loader on
        // its own, so its later checks (an empty `run`, a bad escape)
        // drop it here too.
        *keymap = std::mem::take(keymap)
            .into_iter()
            .filter_map(|(chord, raw)| {
                let single = toml::Table::from_iter([(chord.clone(), raw.clone())]);
                let [(_, parsed)] = toml::Value::Table(single)
                    .try_into::<KeymapConfig>()
                    .ok()?
                    .compile(None)
                    .try_into()
                    .ok()?;
                let toml::Value::Table(raw) = raw else {
                    return None;
                };
                Some((chord, toml::Value::Table(project_binding(&raw, &parsed))))
            })
            .collect();
    }
    diff
}

/// Arrays compare whole: a fallback chain is one setting, not a list
/// of independent ones.
fn diff_tables(effective: &toml::Table, defaults: &toml::Table) -> toml::Table {
    let mut diff = toml::Table::new();
    for (key, value) in effective {
        match (value, defaults.get(key)) {
            (toml::Value::Table(inner), Some(toml::Value::Table(default))) => {
                let nested = diff_tables(inner, default);
                if !nested.is_empty() {
                    diff.insert(key.clone(), toml::Value::Table(nested));
                }
            }
            (value, Some(default)) if value == default => {}
            (value, _) => {
                diff.insert(key.clone(), value.clone());
            }
        }
    }
    diff
}

/// Only the fields named here reach the report: a field added to a
/// kind stays out until it is listed, and a new kind does not compile
/// until it is placed. User-authored text and commands read
/// [`REDACTED`], so the binding's shape survives without its payload.
fn project_binding(raw: &toml::Table, parsed: &BindingValue) -> toml::Table {
    let (shown, redacted): (&[&str], &[&str]) = match parsed {
        BindingValue::Unbind
        | BindingValue::Reload
        | BindingValue::Detach
        | BindingValue::ToggleFullscreen
        | BindingValue::KillSession
        | BindingValue::OpenScrollbackSearch
        | BindingValue::NewSession => (&[], &[]),
        BindingValue::SendString { .. } => (&["escapes"], &["text"]),
        BindingValue::Paste { .. } => (&["from"], &[]),
        BindingValue::Copy { .. } => (&["what"], &[]),
        BindingValue::FontSize { .. } | BindingValue::Scroll { .. } => (&["step"], &[]),
        BindingValue::ScrollToPrompt { .. } | BindingValue::SwitchSession { .. } => (&["to"], &[]),
        BindingValue::Pipe { target, .. } => match target {
            PipeSink::Command(_) | PipeSink::File(_) => (&["source", "ansi"], &["target"]),
            PipeSink::Clipboard | PipeSink::TempFile | PipeSink::Paste => {
                (&["source", "ansi", "target"], &[])
            }
        },
        BindingValue::Run { .. } => (&[], &["command"]),
    };
    let mut projected = toml::Table::new();
    for field in std::iter::once(&"kind").chain(shown) {
        if let Some(value) = raw.get(*field) {
            projected.insert((*field).to_owned(), value.clone());
        }
    }
    for field in redacted {
        if raw.contains_key(*field) {
            projected.insert(
                (*field).to_owned(),
                toml::Value::String(REDACTED.to_owned()),
            );
        }
    }
    projected
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn home() -> HomeCollapse {
        HomeCollapse::new([Path::new("/home/al")])
    }

    fn parsed(text: &str) -> EffectiveConfig {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        let (config, diagnostics) = EffectiveConfig::diagnose(&path, "felis");
        assert_eq!(diagnostics.errors().count(), 0, "{text}");
        config
    }

    #[test]
    fn the_home_prefix_collapses_only_at_a_path_boundary() {
        assert_eq!(home().apply("/home/al/.config/felis"), "~/.config/felis");
        assert_eq!(home().apply("/home/al"), "~");
        assert_eq!(home().apply("/home/alice/x"), "/home/alice/x");
        assert_eq!(home().apply("/home/al.bak/x"), "/home/al.bak/x");
        assert_eq!(home().apply("/home/al: 1 error(s)"), "~: 1 error(s)");
        assert_eq!(home().apply("(\"/home/al\")"), "(\"~\")");
        assert_eq!(
            home().apply("found /home/al/a and /home/al/b"),
            "found ~/a and ~/b"
        );
    }

    #[test]
    fn a_root_home_collapses_nothing() {
        let root = HomeCollapse::new([Path::new("/")]);
        assert_eq!(root.apply("/usr/bin/ssh"), "/usr/bin/ssh");
    }

    #[test]
    fn the_defaults_produce_an_empty_diff() {
        assert!(config_diff(&EffectiveConfig::default()).is_empty());
    }

    #[test]
    fn only_changed_leaves_appear_nested_under_their_tables() {
        let diff = config_diff(&parsed("[font]\nsize_px = 15.0\n"));
        assert_eq!(diff.to_string(), "[font]\nsize_px = 15.0\n");
    }

    #[test]
    fn an_array_that_differs_appears_whole() {
        let diff = config_diff(&parsed(
            "[font]\nfallback = [{ family = \"A\" }, { family = \"B\" }]\n",
        ));
        let fallback = diff["font"]["fallback"].as_array().unwrap();
        assert_eq!(fallback.len(), 2);
    }

    #[test]
    fn other_clients_overlays_are_left_out() {
        let diff = config_diff(&parsed(
            "[client.other]\ntoken = \"secret\"\n[client.felis.font]\nsize_px = 16.0\n",
        ));
        assert!(!diff.contains_key("client"));
        assert_eq!(diff["font"]["size_px"].as_float(), Some(16.0));
    }

    #[test]
    fn user_authored_binding_arguments_are_redacted_and_their_shape_kept() {
        let diff = config_diff(&parsed(
            r#"[keymap]
"ctrl+shift+a" = { kind = "send_string", text = "hunter2\n", escapes = "none" }
"ctrl+shift+b" = { kind = "run", command = ["deploy", "--token", "x"] }
"ctrl+shift+c" = { kind = "pipe", source = "visible", target = { command = ["urlscan"] } }
"ctrl+shift+d" = { kind = "pipe", source = "selection", target = "clipboard" }
"ctrl+shift+e" = { kind = "copy", what = "system" }
"#,
        ));
        let keymap = &diff["keymap"];
        assert_eq!(keymap["ctrl+shift+a"]["text"].as_str(), Some(REDACTED));
        assert_eq!(keymap["ctrl+shift+a"]["kind"].as_str(), Some("send_string"));
        assert_eq!(keymap["ctrl+shift+a"]["escapes"].as_str(), Some("none"));
        assert_eq!(keymap["ctrl+shift+b"]["command"].as_str(), Some(REDACTED));
        assert_eq!(keymap["ctrl+shift+c"]["target"].as_str(), Some(REDACTED));
        assert_eq!(keymap["ctrl+shift+c"]["source"].as_str(), Some("visible"));
        assert_eq!(keymap["ctrl+shift+d"]["target"].as_str(), Some("clipboard"));
        assert_eq!(keymap["ctrl+shift+e"]["what"].as_str(), Some("system"));
    }

    fn sample(inside_felis: bool) -> DoctorReportResult {
        DoctorReportResult {
            failed: 1,
            warned: 0,
            checks: vec![crate::cli_output::CheckObject {
                check: "terminfo".to_owned(),
                status: "fail".to_owned(),
                detail: "no entry | see the how-to\nfor help".to_owned(),
            }],
            environment: EnvironmentObject {
                builds: BuildsObject {
                    cli: "0.1.2 (aaaaaaaaaaaa)".to_owned(),
                    client: Some("0.1.2 (aaaaaaaaaaaa)".to_owned()),
                    daemon: None,
                },
                binary: Some("/nix/store/x-felis/bin/felis".to_owned()),
                os: OsObject {
                    family: "linux".to_owned(),
                    arch: "x86_64".to_owned(),
                    release: Some("NixOS 26.11".to_owned()),
                    kernel: Some("7.2.7".to_owned()),
                },
                display: DisplayObject {
                    session_type: Some("wayland".to_owned()),
                    desktop: Some("niri".to_owned()),
                    wayland: true,
                    x11: false,
                },
                gpu: Some(GpuObject {
                    available: true,
                    name: Some("GPU".to_owned()),
                    backend: Some("vulkan".to_owned()),
                    device_type: Some("discrete_gpu".to_owned()),
                    driver: Some("radv".to_owned()),
                    driver_info: Some("Mesa 26.1.2".to_owned()),
                }),
                locale: LocaleObject {
                    lang: Some("en_US.UTF-8".to_owned()),
                    lc_all: None,
                    lc_ctype: None,
                },
                shell: ShellObject {
                    inside_felis,
                    term: Some("xterm-felis".to_owned()),
                    term_program: Some("felis".to_owned()),
                    term_program_version: Some("0.1.2".to_owned()),
                    colorterm: Some("truecolor".to_owned()),
                    ssh: false,
                },
                fonts: FontsObject {
                    regular: Some("Mono Regular".to_owned()),
                    bold: Some("Mono Bold".to_owned()),
                    italic: Some("Mono Italic".to_owned()),
                    bold_italic: Some("Mono Bold Italic".to_owned()),
                    fallbacks: vec!["Emoji".to_owned()],
                    unavailable: None,
                },
                config: ConfigObject {
                    path: Some("~/.config/felis/config.toml".to_owned()),
                    state: "applied".to_owned(),
                    diff: serde_json::json!({ "font": { "size_px": 15.0 } }),
                },
                logs: vec![LogObject {
                    name: "daemon.log".to_owned(),
                    path: "~/.local/state/felis/daemon.log".to_owned(),
                    present: false,
                }],
            },
        }
    }

    #[test]
    fn the_markdown_report_renders_every_section() {
        insta::assert_snapshot!(markdown(&sample(true), "not running"));
    }

    #[test]
    fn a_report_outside_felis_says_the_session_identity_is_unknown() {
        let text = markdown(&sample(false), "not running");
        assert!(
            text.contains("affected session's terminal identity is unknown"),
            "{text}"
        );
    }

    #[test]
    fn config_text_cannot_close_the_block_it_is_quoted_in() {
        let mut report = sample(true);
        report.environment.config.path = Some("~/a<b>`c/config.toml".to_owned());
        report.environment.config.diff = serde_json::json!({ "window": { "title_prefix": "```" } });
        let text = markdown(&report, "not running");
        assert!(
            text.contains("<code>~/a&lt;b&gt;`c/config.toml</code>"),
            "{text}"
        );
        assert!(text.contains("\n````toml\n"), "{text}");
        assert!(text.contains("\n````\n"), "{text}");

        report.environment.config.state = "absent".to_owned();
        let text = markdown(&report, "not running");
        assert!(
            text.contains("No file at ``~/a<b>`c/config.toml``;"),
            "{text}"
        );
    }

    #[test]
    fn a_pipe_after_a_backslash_stays_inside_its_cell() {
        assert_eq!(cell("a|b"), "a\\|b");
        assert_eq!(cell("a\\|b"), "a\\\\\\|b");
        assert_eq!(cell("a\\b\nc"), "a\\b c");
    }

    #[test]
    fn the_pretty_name_is_read_unquoted() {
        assert_eq!(
            os_release_pretty_name("NAME=NixOS\nPRETTY_NAME=\"NixOS 26.11 (Zokor)\"\n"),
            Some("NixOS 26.11 (Zokor)".to_owned())
        );
        assert_eq!(os_release_pretty_name("NAME=x\n"), None);
    }

    #[test]
    fn every_string_in_the_report_is_collapsed_keys_included() {
        let mut report = sample(true);
        report.environment.shell.term_program = Some("/home/al/bin/term".to_owned());
        report.environment.fonts.unavailable = Some("no font at /home/al/f.ttf".to_owned());
        report.environment.config.diff = serde_json::json!({
            "shader": { "post": { "file": "/home/al/crt.wgsl" } },
            "keymap": { "/home/al": { "kind": "detach" } },
        });
        let redacted = home().redact(report).unwrap();
        let text = serde_json::to_string(&redacted).unwrap();
        assert!(!text.contains("/home/al"), "{text}");
        assert_eq!(
            redacted.environment.shell.term_program.as_deref(),
            Some("~/bin/term")
        );
    }

    #[test]
    fn a_nested_home_collapses_whole() {
        let homes = HomeCollapse::new([Path::new("/home/al"), Path::new("/home/al/sub")]);
        assert_eq!(homes.apply("/home/al/sub/x and /home/al/y"), "~/x and ~/y");
    }

    /// The loader drops these, so there is no shape to redact by.
    #[test]
    fn a_binding_the_loader_drops_is_left_out() {
        let diff = config_diff(&parsed(
            r#"[keymap]
"ctrl+shift+a" = { kind = "copy", what = "system", command = ["leak"] }
"ctrl+shift+b" = { kind = "pipe", source = "visible", target = "leak" }
"not a chord" = { kind = "detach" }
"ctrl+shift+d" = { kind = "run", command = [] }
"ctrl+shift+e" = { kind = "send_string", text = "leak\\q" }
"ctrl+shift+c" = { kind = "detach" }
"#,
        ));
        let keymap = diff["keymap"].as_table().unwrap();
        assert_eq!(keymap.keys().collect::<Vec<_>>(), ["ctrl+shift+c"]);
    }
}

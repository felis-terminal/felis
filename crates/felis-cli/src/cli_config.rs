//! `felis config path|check|show-effective`: inspect configuration without a daemon.
//!
//! These verbs never dial the daemon. `check` exits 1 on document errors
//! and prints diagnostics to stdout.

#![expect(clippy::print_stdout, reason = "these verbs render reports to stdout")]

use std::path::PathBuf;

use clap::Subcommand;
use felis_client_core::config::GUI_CLIENT_ID;
use felis_client_core::{
    ConfigDiagnostics, ConfigSource, Diagnostic, DiagnosticKind, EffectiveConfig, Severity,
};

use crate::cli_output::{
    CheckResult, ConfigPathResult, DiagnosticObject, EffectiveConfigResult, ErrorKind, Format,
    PointFormat, Reporter,
};

/// The default is a constant, not a binary name: overlays key on
/// `[client.felis]` whatever the GUI binary is called, so a rename must
/// not orphan everyone's section (docs/reference/config.md
/// "Per-client overrides (`[client.<name>]`)").
#[derive(Debug, clap::Args)]
pub(crate) struct ClientScope {
    /// Which `[client.<id>]` overlay to fold in. Defaults to the GUI client id.
    ///
    /// Other `[client.*]` sections are untouched and not checked because
    /// their schema may differ.
    #[arg(long, value_name = "ID", default_value = GUI_CLIENT_ID)]
    client: String,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ConfigOp {
    /// Print where felis looks for `config.toml`.
    ///
    /// The path is reported whether or not the file exists: a
    /// first-time user has none, and "where would it go" is the
    /// question they are asking.
    Path {
        #[command(flatten)]
        output: PointFormat,
    },
    /// Validate the config file and print every problem it has.
    ///
    /// Reports all errors and warnings rather than stopping at the first.
    /// Exits 0 when clean or warnings only, 1 when any error is present.
    /// A missing default file exits 0; a missing `--config` file is an error.
    Check {
        #[command(flatten)]
        scope: ClientScope,
        #[command(flatten)]
        output: PointFormat,
    },
    /// Print the configuration felis would actually use: defaults
    /// filled in and the selected client's overlay folded in.
    ///
    /// The human framing is TOML, the format you wrote it in, so what
    /// you read is what you would edit.
    #[command(name = "show-effective")]
    ShowEffective {
        #[command(flatten)]
        scope: ClientScope,
        #[command(flatten)]
        output: PointFormat,
    },
}

impl ConfigOp {
    pub(crate) const fn format(&self) -> Format {
        match self {
            Self::Path { output }
            | Self::Check { output, .. }
            | Self::ShowEffective { output, .. } => output.format,
        }
    }
}

pub(crate) fn run(op: ConfigOp, source: &ConfigSource) -> i32 {
    match op {
        ConfigOp::Path { output } => cmd_path(&Reporter::point(output.format), source),
        ConfigOp::Check { scope, output } => {
            cmd_check(&Reporter::point(output.format), source, &scope.client)
        }
        ConfigOp::ShowEffective { scope, output } => {
            cmd_show_effective(&Reporter::point(output.format), source, &scope.client)
        }
    }
}

/// The one failure this module has: no discoverable home directory (a
/// sandbox with no `$HOME`). An explicit selection never hits it.
fn resolve_path(out: &Reporter, source: &ConfigSource) -> Result<PathBuf, i32> {
    source.path().ok_or_else(|| {
        out.fail(
            ErrorKind::Internal,
            "no config directory: felis found no home directory to resolve one from",
        )
    })
}

fn cmd_path(out: &Reporter, source: &ConfigSource) -> i32 {
    let path = match resolve_path(out, source) {
        Ok(path) => path,
        Err(code) => return code,
    };
    let exists = path.is_file();
    if out.machine() {
        out.result(&ConfigPathResult {
            path: path.display().to_string(),
            exists,
        });
    } else {
        // The bare path alone, so it pipes into `$EDITOR` unfiltered.
        println!("{}", path.display());
    }
    0
}

fn cmd_check(out: &Reporter, source: &ConfigSource, client: &str) -> i32 {
    let path = match resolve_path(out, source) {
        Ok(path) => path,
        Err(code) => return code,
    };
    let exists = path.is_file();
    let (_config, diagnostics) = EffectiveConfig::diagnose_source(source, client);
    let errors = diagnostics.errors().count();
    let warnings = diagnostics.warnings().count();

    if out.machine() {
        out.result(&CheckResult {
            path: path.display().to_string(),
            exists,
            client: client.to_owned(),
            errors: errors as u64,
            warnings: warnings as u64,
            diagnostics: diagnostics.iter().map(diagnostic_object).collect(),
        });
    } else {
        print_check(&path, exists, client, &diagnostics, errors, warnings);
    }
    i32::from(errors > 0)
}

fn print_check(
    path: &std::path::Path,
    exists: bool,
    client: &str,
    diagnostics: &ConfigDiagnostics,
    errors: usize,
    warnings: usize,
) {
    println!("config: {}", path.display());
    println!("client: {client}");
    if !exists && errors == 0 {
        // "No problems" and "no file" are different answers; only one
        // means the user's settings are being applied. A selected
        // file that is absent carries an error diagnostic instead.
        println!("no config file — felis uses its built-in defaults");
        return;
    }
    for d in diagnostics.iter() {
        println!("{:<8} {d}", severity_token(d.severity));
    }
    println!("{errors} error(s), {warnings} warning(s)");
}

fn cmd_show_effective(out: &Reporter, source: &ConfigSource, client: &str) -> i32 {
    let path = match resolve_path(out, source) {
        Ok(path) => path,
        Err(code) => return code,
    };
    let (config, diagnostics) = EffectiveConfig::diagnose_source(source, client);
    if let Some(first) = diagnostics.errors().next() {
        // Unlike `check`, nothing honest can be printed past an error:
        // `diagnose` hands back the *defaults*, and printing those as
        // "effective" would claim the user's file was applied.
        return out.fail(
            ErrorKind::InvalidRequest,
            format!("{first}\nrun `felis config check` for the full report"),
        );
    }
    if out.machine() {
        out.result(&EffectiveConfigResult {
            path: path.display().to_string(),
            client: client.to_owned(),
            config: crate::cli_output::body_value(&config),
        });
    } else {
        match toml::to_string_pretty(&config) {
            Ok(text) => print!("{text}"),
            Err(err) => return out.fail(ErrorKind::Internal, err),
        }
    }
    0
}

fn diagnostic_object(d: &Diagnostic) -> DiagnosticObject {
    DiagnosticObject {
        severity: severity_token(d.severity).to_owned(),
        kind: kind_token(d.kind).to_owned(),
        key: d.key.clone(),
        message: d.message.clone(),
    }
}

const fn severity_token(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
    }
}

/// The closed set a consumer branches on (docs/reference/config.md).
const fn kind_token(kind: DiagnosticKind) -> &'static str {
    match kind {
        DiagnosticKind::Io => "io",
        DiagnosticKind::Parse => "parse",
        DiagnosticKind::UnknownKey => "unknown_key",
        DiagnosticKind::Value => "value",
        DiagnosticKind::MissingFile => "missing_file",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tokens are the machine contract; an enum rename must not
    /// retype a category.
    #[test]
    fn diagnostic_tokens_are_the_documented_set() {
        for (kind, token) in [
            (DiagnosticKind::Io, "io"),
            (DiagnosticKind::Parse, "parse"),
            (DiagnosticKind::UnknownKey, "unknown_key"),
            (DiagnosticKind::Value, "value"),
            (DiagnosticKind::MissingFile, "missing_file"),
        ] {
            assert_eq!(kind_token(kind), token);
        }
        assert_eq!(severity_token(Severity::Error), "error");
        assert_eq!(severity_token(Severity::Warning), "warning");
    }

    /// The overlay default is the GUI client's constant, not the binary
    /// name.
    #[test]
    fn the_client_scope_defaults_to_the_gui_overlay_constant() {
        use clap::Parser as _;

        #[derive(clap::Parser)]
        struct Probe {
            #[command(flatten)]
            scope: ClientScope,
        }
        assert_eq!(Probe::parse_from(["probe"]).scope.client, GUI_CLIENT_ID);
        assert_eq!(
            Probe::parse_from(["probe", "--client", "tui"]).scope.client,
            "tui"
        );
    }
}

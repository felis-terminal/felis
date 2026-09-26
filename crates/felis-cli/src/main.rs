//! `felis`: light, GPU-free front-door binary.
//!
//! Runs headless verbs in-process without wgpu or winit, and execs a frontend
//! binary for window launches (docs/reference/ipc.md "CLI clients").

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use clap::{CommandFactory as _, Parser, Subcommand};
use felis_client_core::ConfigSource;
use felis_protocol::messages::{ResolvedId, SessionInfo};
use felis_transport::logging::{self, Console};

mod cli_bridge;
mod cli_bridge_json;
mod cli_completions;
mod cli_config;
mod cli_daemon;
mod cli_doctor;
mod cli_mangen;
mod cli_notifications;
mod cli_output;
#[cfg(all(test, feature = "schema"))]
mod cli_schema;
mod cli_sessions;
mod cli_version;
mod conn;
mod timestamp;

pub(crate) use felis_client_core::session_id::{SessionPrefixError, validate_session_id_prefix};

/// The instant a `--timeout <secs>` wait gives up at. A timeout too far
/// out to represent as an `Instant` is no deadline, the same as waiting
/// forever.
pub(crate) fn timeout_deadline(timeout_secs: Option<u64>) -> Option<tokio::time::Instant> {
    timeout_secs.and_then(|secs| {
        tokio::time::Instant::now().checked_add(std::time::Duration::from_secs(secs))
    })
}

/// One wording for "which session?" whether the prefix resolved
/// daemon-side or against a roster.
pub(crate) fn session_id_from_resolved(
    resolved: ResolvedId,
    prefix: &str,
) -> Result<u128, SessionPrefixError> {
    match resolved {
        ResolvedId::Ok { id } => Ok(id),
        ResolvedId::NoMatch => Err(SessionPrefixError::NoMatch {
            prefix: prefix.to_owned(),
        }),
        ResolvedId::Ambiguous { matches } => Err(SessionPrefixError::Ambiguous {
            prefix: prefix.to_owned(),
            matches: matches as usize,
        }),
    }
}

pub(crate) fn build_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")
}

/// Owns the unqualified launch; alternate frontends live behind
/// `felis frontend <name>` (docs/reference/cli.md
/// "Alternate frontends: `felis frontend <name>`").
const DEFAULT_FRONTEND_BIN: &str = "felis-client";

/// `None` launches the default frontend (docs/reference/ipc.md "CLI
/// clients").
#[derive(Debug, Subcommand)]
enum Cmd {
    /// List, drive, and inspect sessions without opening a window.
    ///
    /// Every verb talks to the daemon directly and exits; `--format json`/`jsonl` is
    /// the stable scripting surface. See `felis sessions help <verb>`.
    Sessions {
        #[command(subcommand)]
        op: cli_sessions::SessionOp,
    },
    /// Stream desktop notifications from sessions.
    ///
    /// Relays OSC 9 / 99 / 777 notifications; felis draws no popups itself.
    /// Pipe into an external notifier (`notify-send`, `terminal-notifier`).
    Notifications {
        #[command(subcommand)]
        op: cli_notifications::NotificationOp,
    },
    /// Inspect the config file felis reads without dialing a daemon.
    ///
    /// `path` prints the file location, `check` validates diagnostics,
    /// and `show-effective` prints the merged configuration.
    Config {
        #[command(subcommand)]
        op: cli_config::ConfigOp,
    },
    /// Check every layer felis needs and diagnose failures.
    ///
    /// Inspects daemon, config, terminfo, GPU, clipboard, and SSH.
    /// Exits 1 on failures; unreachable daemons are findings, not exit 2.
    Doctor {
        #[command(flatten)]
        output: cli_output::PointFormat,
    },
    /// Compare this build against the GUI client and running daemon.
    ///
    /// Reports three rows (`cli`, `client`, `daemon`) with build identities.
    /// Informational only; does not enforce version matching.
    Version {
        #[command(flatten)]
        output: cli_output::PointFormat,
    },
    /// Inspect the daemon behind sessions.
    ///
    /// Reports build, wire version, and resource usage against ceilings.
    /// Never starts a daemon.
    Daemon {
        #[command(subcommand)]
        op: cli_daemon::DaemonOp,
    },
    /// Open a window attached to an existing session.
    ///
    /// Bare `felis` is the create-new counterpart. The globals reach
    /// the window and belong before the verb (`felis --config
    /// work.toml attach <id>`); see `felis --help`.
    Attach {
        /// Hex id or any unique prefix.
        #[arg(value_name = "ID-OR-PREFIX", value_parser = crate::validate_session_id_prefix)]
        id: String,
    },
    /// Act on the window this command runs in.
    ///
    /// The object is the window, not the session behind it: `retarget`
    /// re-points it at another daemon. Run from inside a felis window;
    /// outside one there is nothing to act on.
    Window {
        #[command(subcommand)]
        op: WindowOp,
    },
    /// Re-point this window at the daemon on another host, over SSH.
    ///
    /// The window re-dials the destination and lands there; closing it
    /// leaves the remote session running. `felis --host <dest>` is the
    /// other verb: it opens a new window on that host instead.
    Ssh {
        /// Any destination `ssh` accepts: `user@host`, an
        /// `~/.ssh/config` alias, or an `ssh://user@host:port` URI,
        /// passed verbatim.
        #[arg(value_name = "user@host")]
        dest: String,
        /// Extra token passed verbatim to `ssh` before the destination
        /// (repeatable): `--ssh-arg=-p --ssh-arg=2222`. Handy for an
        /// ad-hoc VM not worth an `~/.ssh/config` entry; felis never
        /// interprets the tokens.
        #[arg(long = "ssh-arg", value_name = "TOKEN")]
        ssh_arg: Vec<String>,
        #[command(flatten)]
        attachment: cli_sessions::AttachmentTarget,
        #[command(flatten)]
        flags: cli_sessions::RetargetFlags,
    },
    /// Launch an alternate frontend: `felis frontend <name> …` execs
    /// `felis-<name>` with the remaining arguments verbatim.
    ///
    /// No global reaches it: `--config` and the carrier before the name
    /// are refused; write them after it instead (see `felis --help`).
    Frontend {
        /// Frontend name: `felis frontend tui` execs `felis-tui`.
        #[arg(value_name = "NAME", value_parser = validate_frontend_name)]
        name: String,
        /// Arguments handed to the frontend verbatim, flags included.
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        args: Vec<OsString>,
    },
    /// Speak JSON lines on stdin/stdout for a non-Rust client.
    ///
    /// Persistent stdio bridge relaying JSONL requests and replies to the daemon.
    /// Stderr carries log lines only, never protocol. Exits if daemon is unreachable.
    Bridge,
    /// Print a shell completion script to stdout.
    ///
    /// Supports bash, elvish, fish, powershell, and zsh. The fish and zsh
    /// scripts also complete session IDs dynamically against running
    /// daemons.
    Completions {
        /// Target shell.
        shell: clap_complete::Shell,
    },
    /// Hidden machine-readable session listing for shell completion
    /// scripts. Emits one line per session as `<id>\t<description>`
    /// (description = trimmed title, then ` — <cwd>` when present).
    /// Silent (exit 0, no output) when the daemon is unreachable, so a
    /// `<TAB>` against a cold daemon surfaces no noise mid-completion.
    #[command(name = "__complete-sessions", hide = true)]
    CompleteSessions,
    /// Hidden man-page generator for the package build
    /// (`cli_mangen.rs`): renders `felis(1)` and one page per visible
    /// subcommand from these clap definitions into DIR.
    #[command(name = "__mangen", hide = true)]
    Mangen {
        /// Output directory for the roff pages (created if missing).
        dir: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum WindowOp {
    /// Re-point this window at a daemon on this machine.
    ///
    /// Bare, it re-dials the default local daemon: the way back from
    /// `felis ssh`. A SOCKET path names a second local daemon.
    Retarget {
        /// Socket of the local daemon to re-dial. Absent: the default
        /// local daemon.
        #[arg(value_name = "SOCKET")]
        socket: Option<PathBuf>,
        #[command(flatten)]
        attachment: cli_sessions::AttachmentTarget,
        #[command(flatten)]
        flags: cli_sessions::RetargetFlags,
    },
}

/// A separator or a leading dash would turn the `felis-<name>` exec
/// into "run this path" or "pass this flag".
fn validate_frontend_name(name: &str) -> Result<String, String> {
    if name.is_empty() {
        return Err("frontend name is empty".to_owned());
    }
    if name.starts_with('-')
        || name
            .chars()
            .any(|c| std::path::is_separator(c) || c == '.' || c.is_whitespace())
    {
        return Err(format!(
            "`{name}` is not a frontend name: `felis frontend <name>` execs `felis-<name>`, \
             so the name carries no path"
        ));
    }
    Ok(name.to_owned())
}

/// felis: a terminal whose sessions outlive their windows.
///
/// Opens a window on a fresh session, attaches to an existing session,
/// or drives the daemon headlessly via `sessions` and `notifications`.
#[derive(Debug, Parser)]
#[command(name = "felis")]
struct Cli {
    /// Print this binary's own build and exit.
    ///
    /// Self-report only: no subprocess, no daemon, no network, so it
    /// refuses the globals rather than ignore them; `felis version`
    /// compares all three builds.
    #[arg(short = 'V', long)]
    version: bool,
    /// Subcommand. When a headless verb, dispatches to the typed IPC
    /// path and exits; when a frontend-launch verb (`attach`, an
    /// external frontend) or absent, execs a frontend binary.
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Connect to a remote daemon over SSH (`ssh <host> felis-daemon
    /// relay`), for window launches and headless verbs alike. Only
    /// `sessions spawn` and window launches start a cold daemon there;
    /// the read and drive verbs exit 2 instead, `version` and `doctor`
    /// report it as not running, and completion never dials.
    #[arg(long, value_name = "user@host")]
    host: Option<String>,
    /// Extra token passed verbatim to `ssh` before the destination (repeatable).
    ///
    /// felis passes tokens directly without interpretation; only `ssh` parses them.
    /// Requires `--host`.
    #[arg(long = "ssh-arg", value_name = "TOKEN", requires = "host")]
    ssh_arg: Vec<String>,
    /// Use this daemon socket instead of the default per-user path.
    ///
    /// Allows multiple daemons to coexist under one user.
    /// Mutually exclusive with `--host`.
    #[arg(long, value_name = "PATH", conflicts_with = "host")]
    socket: Option<PathBuf>,
    /// Read this `config.toml` instead of the platform default.
    ///
    /// The selected file must exist. Applies to window launches, `config`,
    /// and `doctor`; other verbs refuse it.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Run this program in the new window instead of the default `$SHELL` (`xterm -e`).
    ///
    /// Arguments after `--` are taken literally as argv. Ignored with a subcommand.
    // `last = true` keeps bare words from bypassing subcommand dispatch.
    #[arg(last = true, value_name = "CMD")]
    command: Vec<String>,
}

impl Cli {
    const fn machine_output(&self) -> bool {
        verb_reporter(self.cmd.as_ref()).machine()
    }
}

/// The reporter a verb answers in: its class and its `--format`. Every
/// failure past a successful parse is framed through one of these, the
/// pre-dispatch steps included, so a machine consumer never meets prose
/// on a verb it asked for objects from (docs/reference/cli.md "Machine
/// output").
const fn verb_reporter(cmd: Option<&Cmd>) -> cli_output::Reporter {
    use cli_output::Format::Human;
    use cli_output::Reporter;
    match cmd {
        // A stream verb needs a stream-framed terminal rather than
        // point framing (docs/reference/cli.md "The envelope").
        Some(Cmd::Sessions { op }) => {
            if matches!(
                op,
                cli_sessions::SessionOp::Capture { .. } | cli_sessions::SessionOp::Search { .. }
            ) {
                Reporter::stream(op.format())
            } else {
                Reporter::point(op.format())
            }
        }
        Some(Cmd::Notifications { op }) => Reporter::stream(op.format()),
        Some(Cmd::Config { op }) => Reporter::point(op.format()),
        Some(Cmd::Daemon { op }) => Reporter::point(op.format()),
        Some(Cmd::Doctor { output } | Cmd::Version { output }) => Reporter::point(output.format),
        Some(
            Cmd::Window {
                op: WindowOp::Retarget { flags, .. },
            }
            | Cmd::Ssh { flags, .. },
        ) => Reporter::point(flags.output.format),
        // The exempt verbs and the window launches frame nothing.
        _ => Reporter::point(Human),
    }
}

/// A step that runs before the verb body and can still fail: the
/// failure wears the verb's framing and the kind's exit code, never an
/// `anyhow` line and exit `1`.
fn framed<T>(out: &cli_output::Reporter, kind: cli_output::ErrorKind, step: Result<T>) -> T {
    match step {
        Ok(value) => value,
        Err(err) => std::process::exit(out.fail(kind, format!("{err:#}"))),
    }
}

fn dialed(cli: &Cli, dials: Dials, out: &cli_output::Reporter) -> felis_client_core::Reconnector {
    framed(
        out,
        cli_output::ErrorKind::DaemonUnreachable,
        dial_target(cli, dials),
    )
}

fn verb_runtime(out: &cli_output::Reporter) -> tokio::runtime::Runtime {
    framed(out, cli_output::ErrorKind::Internal, build_runtime())
}

fn main() -> Result<()> {
    let mut cli = Cli::parse();

    // Handled here, not by clap's `ArgAction::Version`: that action
    // short-circuits at the flag and would exit 0 on `felis --version
    // sessions lst`, hiding the typo.
    if cli.version {
        if let Some(conflict) = version_conflict(&cli) {
            Cli::command()
                .error(clap::error::ErrorKind::ArgumentConflict, conflict)
                .exit();
        }
        #[expect(
            clippy::print_stdout,
            reason = "the version line IS this flag's contract"
        )]
        {
            println!("felis {}", cli_version::SELF_VERSION_LINE);
        }
        std::process::exit(0);
    }

    // Log to stderr because stdout carries the machine-readable contract.
    // Machine formatting silences console logging so errors remain a single
    // typed JSON object; RUST_LOG overrides this default.
    let fallback = if cli.machine_output() { "off" } else { "warn" };
    logging::init(Console::Stderr, None, fallback);

    let out = verb_reporter(cli.cmd.as_ref());

    // Refused before resolution: a verb that reads no config owes the
    // caller that sentence, not a resolution failure of a flag it was
    // going to refuse anyway.
    if cli.config.is_some()
        && let Some(verb) = verb_reading_no_config(cli.cmd.as_ref())
    {
        std::process::exit(out.fail(
            cli_output::ErrorKind::Usage,
            format!(
                "{verb}: --config selects the config.toml a window launch, `felis config`, or \
                 `felis doctor` reads; this verb reads none. Drop the flag."
            ),
        ));
    }
    // Read before `cli.cmd` moves into the match.
    let global_carrier = cli.host.is_some() || cli.socket.is_some() || !cli.ssh_arg.is_empty();
    if global_carrier && let Some((verb, tail)) = verb_refusing_carrier(cli.cmd.as_ref()) {
        std::process::exit(cli_sessions::reject_global_carrier(&out, &verb, tail));
    }
    // Resolved once, here: the frontend receives the absolute form, so
    // it never has to agree with this process about what a relative
    // path meant (docs/reference/cli.md "Global options").
    let config_source = framed(
        &out,
        cli_output::ErrorKind::Usage,
        resolve_config_source(cli.config.as_deref()),
    );
    let dials = dials(cli.cmd.as_ref());

    // Taken, not moved into the match: the arms still read the globals
    // off `cli` to build the daemon target and the frontend line.
    match cli.cmd.take() {
        Some(Cmd::Sessions { op }) => {
            let target = dialed(&cli, dials, &out);
            let runtime = verb_runtime(&out);
            let code = framed(
                &out,
                cli_output::ErrorKind::Internal,
                cli_sessions::run(&runtime, op, &target),
            );
            std::process::exit(code);
        }
        Some(Cmd::Config { op }) => {
            std::process::exit(cli_config::run(op, &config_source));
        }
        Some(Cmd::Doctor { output }) => {
            // Unlike the config verbs, `doctor` dials, so `--host` makes
            // its daemon row report the daemon over there; every other
            // row still describes *this* machine (docs/reference/cli.md
            // "Doctor").
            let target = framed(
                &out,
                cli_output::ErrorKind::DaemonUnreachable,
                dial_resolved(&cli, dials),
            );
            let runtime = verb_runtime(&out);
            let code = cli_doctor::run(&runtime, &output, &target, &config_source, || {
                frontend_program(DEFAULT_FRONTEND_BIN)
            });
            std::process::exit(code);
        }
        Some(Cmd::Version { output }) => {
            let target = dialed(&cli, dials, &out);
            let runtime = verb_runtime(&out);
            let code = cli_version::run(&runtime, &output, &target, || {
                frontend_program(DEFAULT_FRONTEND_BIN)
            });
            std::process::exit(code);
        }
        Some(Cmd::Daemon { op }) => {
            let target = dialed(&cli, dials, &out);
            let runtime = verb_runtime(&out);
            let code = framed(
                &out,
                cli_output::ErrorKind::Internal,
                cli_daemon::run(&runtime, op, &target),
            );
            std::process::exit(code);
        }
        Some(Cmd::Notifications { op }) => {
            let target = dialed(&cli, dials, &out);
            let runtime = verb_runtime(&out);
            let code = framed(
                &out,
                cli_output::ErrorKind::Internal,
                cli_notifications::run(&runtime, op, &target),
            );
            std::process::exit(code);
        }
        Some(Cmd::Window {
            op:
                WindowOp::Retarget {
                    socket,
                    attachment,
                    flags,
                },
        }) => {
            let carrier = framed(
                &out,
                cli_output::ErrorKind::Usage,
                cli_sessions::local_carrier(socket.as_deref()),
            );
            let code = retarget(&out, "felis window retarget", carrier, attachment, flags);
            std::process::exit(code);
        }
        Some(Cmd::Ssh {
            dest,
            ssh_arg,
            attachment,
            flags,
        }) => {
            let carrier = felis_protocol::messages::RetargetCarrier::Ssh {
                destination: dest,
                ssh_args: ssh_arg,
            };
            let code = retarget(&out, "felis ssh", carrier, attachment, flags);
            std::process::exit(code);
        }
        Some(Cmd::Bridge) => {
            let target = dialed(&cli, dials, &out);
            let runtime = verb_runtime(&out);
            let code = cli_bridge::run(&runtime, &target)?;
            std::process::exit(code);
        }
        Some(Cmd::Completions { shell }) => {
            cli_completions::emit(shell);
            std::process::exit(0);
        }
        Some(Cmd::CompleteSessions) => {
            if dials == Dials::LocalOnly && cli.host.is_some() {
                std::process::exit(0);
            }
            // Runs on every <TAB>: a resolution failure must not drop
            // error text into the candidate list, so the target is
            // resolved by hand instead of `?`-propagated.
            let runtime = verb_runtime(&out);
            let code = match dial_target(&cli, dials) {
                Ok(target) => cli_completions::run_complete_sessions(&runtime, &target),
                Err(_) => 0,
            };
            std::process::exit(code);
        }
        Some(Cmd::Mangen { dir }) => {
            cli_mangen::generate(&dir).context("write man pages")?;
            std::process::exit(0);
        }
        Some(Cmd::Frontend { name, args }) => {
            let mut forwarded = Vec::with_capacity(args.len());
            forwarded.extend(args.iter().cloned());
            Err(exec_frontend(&format!("felis-{name}"), &forwarded))
        }
        Some(Cmd::Attach { id }) => {
            require_selected_config(&config_source);
            let mut args = global_passthrough(&cli, &config_source);
            args.push("attach".into());
            args.push(id.into());
            Err(exec_frontend(DEFAULT_FRONTEND_BIN, &args))
        }
        None => {
            require_selected_config(&config_source);
            let mut args = global_passthrough(&cli, &config_source);
            if !cli.command.is_empty() {
                args.push("--".into());
                args.extend(cli.command.iter().map(OsString::from));
            }
            Err(exec_frontend(DEFAULT_FRONTEND_BIN, &args))
        }
    }
}

/// Which globals name the daemon a verb dials, per the carrier column
/// of the frozen matrix (docs/reference/cli.md "Global options").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dials {
    /// `--host`, `--ssh-arg` and `--socket` name the daemon.
    Globals,
    /// The local socket only, `--host` there yielding no candidates.
    LocalOnly,
}

/// `__complete-sessions` runs on every `<TAB>` and never runs `ssh`: an
/// installed completion script forwards whatever it was generated
/// with, so its carrier tokens are dropped rather than trusted
/// (docs/explanation/architecture/control-surfaces.md).
const fn dials(cmd: Option<&Cmd>) -> Dials {
    match cmd {
        Some(Cmd::CompleteSessions) => Dials::LocalOnly,
        _ => Dials::Globals,
    }
}

fn dial_target(cli: &Cli, dials: Dials) -> Result<felis_client_core::Reconnector> {
    dial_resolved(cli, dials).map(|resolved| resolved.target)
}

fn dial_resolved(cli: &Cli, dials: Dials) -> Result<conn::Resolved> {
    match dials {
        Dials::Globals => conn::resolve(cli.host.as_deref(), &cli.ssh_arg, cli.socket.as_deref()),
        Dials::LocalOnly => conn::resolve(None, &[], cli.socket.as_deref()),
    }
}

fn retarget(
    out: &cli_output::Reporter,
    verb: &'static str,
    carrier: felis_protocol::messages::RetargetCarrier,
    attachment: cli_sessions::AttachmentTarget,
    flags: cli_sessions::RetargetFlags,
) -> i32 {
    // No global carrier survived `verb_refusing_carrier`, so the
    // window's daemon is the local default.
    let target = framed(
        out,
        cli_output::ErrorKind::DaemonUnreachable,
        conn::resolve(None, &[], None),
    )
    .target;
    let runtime = verb_runtime(out);
    framed(
        out,
        cli_output::ErrorKind::Internal,
        cli_sessions::run_retarget(
            &runtime,
            &target,
            verb,
            cli_sessions::RetargetArgs::from_flags(carrier, attachment, flags),
        ),
    )
}

/// A relative `--config` names a file relative to where the user typed
/// the command, not to wherever the frontend later finds itself, so it
/// is made absolute before it can be forwarded or reported.
fn resolve_config_source(explicit: Option<&std::path::Path>) -> Result<ConfigSource> {
    let Some(path) = explicit else {
        return Ok(ConfigSource::Default);
    };
    if path.is_absolute() {
        return Ok(ConfigSource::Explicit(path.to_path_buf()));
    }
    let cwd = std::env::current_dir().context("resolve --config against the current directory")?;
    let resolved = cwd.join(path);
    // A Windows drive-relative path (`C:felis.toml`) carries a prefix without
    // a root, so `cwd.join` does not make it absolute and would cause the
    // frontend to resolve it against an unpredictable directory.
    if !resolved.is_absolute() {
        anyhow::bail!(
            "--config {}: drive-relative paths cannot be made absolute; write the full path",
            path.display()
        );
    }
    Ok(ConfigSource::Explicit(resolved))
}

/// Validate before exec so a typo'd `--config` fails immediately; the
/// frontend falls back to defaults when the document is unusable, opening
/// a window without user settings (docs/reference/cli.md "Global options").
fn require_selected_config(source: &ConfigSource) {
    let Err(err) = source.require_selection() else {
        return;
    };
    let message = err
        .diagnostics()
        .errors()
        .next()
        .map_or_else(|| "config unusable".to_owned(), ToString::to_string);
    std::process::exit(
        cli_output::Reporter::point(cli_output::Format::Human)
            .fail(cli_output::ErrorKind::Usage, message),
    );
}

/// Verbs that read no config file, named for the refusal.
/// Refuses `--config` to prevent silent drops (docs/reference/cli.md "Global options").
fn verb_reading_no_config(cmd: Option<&Cmd>) -> Option<String> {
    let verb = match cmd? {
        Cmd::Attach { .. } | Cmd::Config { .. } | Cmd::Doctor { .. } => return None,
        Cmd::Sessions { .. } => "felis sessions",
        Cmd::Notifications { .. } => "felis notifications",
        Cmd::Version { .. } => "felis version",
        Cmd::Daemon { .. } => "felis daemon",
        Cmd::Window {
            op: WindowOp::Retarget { .. },
        } => "felis window retarget",
        Cmd::Ssh { .. } => "felis ssh",
        Cmd::Bridge => "felis bridge",
        Cmd::Completions { .. } => "felis completions",
        Cmd::CompleteSessions => "felis __complete-sessions",
        Cmd::Mangen { .. } => "felis __mangen",
        // Like the carrier globals: `felis` does not know how
        // `felis-<name>` spells its own config flag.
        Cmd::Frontend { name, .. } => return Some(format!("felis frontend {name}")),
    };
    Some(verb.to_owned())
}

/// The verbs that dial no daemon the global carrier could name, each
/// with the tail that says why. Refused there rather than ignored, for
/// the reason `--config` is (docs/reference/cli.md "Global options").
fn verb_refusing_carrier(cmd: Option<&Cmd>) -> Option<(String, &'static str)> {
    let (verb, tail) = match cmd? {
        // Forwarded to the frontend, or naming the daemon this verb
        // dials; `__complete-sessions` answers `--host` with an empty
        // candidate list instead (docs/reference/cli.md "Other verbs").
        Cmd::Attach { .. }
        | Cmd::Sessions { .. }
        | Cmd::Notifications { .. }
        | Cmd::Daemon { .. }
        | Cmd::Version { .. }
        | Cmd::Doctor { .. }
        | Cmd::Bridge
        | Cmd::CompleteSessions => return None,
        Cmd::Config { .. } => (
            "felis config".to_owned(),
            "these verbs dial nothing — they read this machine's config.toml, which is the \
             client's file and not any daemon's. Drop the flag; to inspect a remote machine's \
             config, run `felis config` there.",
        ),
        Cmd::Frontend { name, .. } => (
            format!("felis frontend {name}"),
            "an external frontend dials its own daemon by its own flags, which `felis` does not \
             know. Put them after the frontend name instead.",
        ),
        // Two hosts on one line and no way to tell which is which:
        // `felis --host a ssh b`.
        Cmd::Window {
            op: WindowOp::Retarget { .. },
        } => (
            "felis window retarget".to_owned(),
            "this verb is addressed to the window's own daemon and carries its own destination. \
             Put the destination after the verb instead.",
        ),
        Cmd::Ssh { .. } => (
            "felis ssh".to_owned(),
            "this verb is addressed to the window's own daemon and carries its own destination. \
             Put the destination after the verb instead.",
        ),
        Cmd::Completions { .. } => (
            "felis completions".to_owned(),
            "this verb dials nothing — it prints a completion script built from this binary's \
             own grammar. Drop the flag.",
        ),
        Cmd::Mangen { .. } => (
            "felis __mangen".to_owned(),
            "this verb dials nothing — it writes man pages from this binary's own grammar. Drop \
             the flag.",
        ),
    };
    Some((verb, tail))
}

/// `--version` answers before any verb dispatch, config resolution, or
/// dial, so every global on the line would be dropped rather than
/// honored (docs/reference/cli.md "Global options"). Clap's own error
/// kind, not a framed refusal: the flag chooses no format.
const fn version_conflict(cli: &Cli) -> Option<&'static str> {
    if cli.cmd.is_some() || !cli.command.is_empty() {
        return Some(
            "`--version` reports this build and runs nothing else; drop it to run the command, or use `felis version` to compare builds",
        );
    }
    if cli.config.is_some() || cli.host.is_some() || cli.socket.is_some() || !cli.ssh_arg.is_empty()
    {
        return Some(
            "`--version` reports this build and runs nothing else: it reads no config.toml and dials no daemon; drop --config/--host/--socket/--ssh-arg, or use `felis version` to compare builds",
        );
    }
    None
}

/// felis-cli owns the canonical CLI surface; the frontend re-parses
/// this narrower subset, so the flags are rebuilt explicitly rather
/// than forwarded as a raw argv slice.
fn global_passthrough(cli: &Cli, config_source: &ConfigSource) -> Vec<OsString> {
    let mut args = Vec::new();
    if let ConfigSource::Explicit(path) = config_source {
        args.push("--config".into());
        args.push(path.into());
    }
    if let Some(socket) = &cli.socket {
        args.push("--socket".into());
        args.push(socket.into());
    }
    if let Some(host) = &cli.host {
        args.push("--host".into());
        args.push(host.into());
    }
    for token in &cli.ssh_arg {
        args.push("--ssh-arg".into());
        args.push(token.into());
    }
    args
}

/// Resolution is `current_exe()` sibling first, then PATH
/// (docs/reference/cli.md "Alternate frontends: `felis frontend <name>`")
/// so a packaged front door reaches the frontend shipped alongside it
/// before any ambient build. On success this never returns; the returned
/// error is always a launch failure.
fn exec_frontend(bin_stem: &str, args: &[OsString]) -> anyhow::Error {
    let program = frontend_program(bin_stem);
    let mut cmd = std::process::Command::new(&program);
    cmd.args(args);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        wrap_exec_err(bin_stem, cmd.exec())
    }
    #[cfg(not(unix))]
    {
        // No `exec` off Unix: propagate the frontend's exit code.
        match cmd.status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(err) => wrap_exec_err(bin_stem, err),
        }
    }
}

fn frontend_program(bin_stem: &str) -> OsString {
    let name = format!("{bin_stem}{}", std::env::consts::EXE_SUFFIX);
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(&name)))
        .filter(|sibling| sibling.is_file())
        .map_or_else(|| OsString::from(&name), Into::into)
}

/// A missing default GUI frontend means "this is the headless build";
/// a missing external frontend means "no such frontend / typo".
fn wrap_exec_err(bin_stem: &str, err: std::io::Error) -> anyhow::Error {
    if err.kind() == std::io::ErrorKind::NotFound {
        if bin_stem == DEFAULT_FRONTEND_BIN {
            anyhow!(
                "the felis GUI frontend ({bin_stem}) is not installed.\n\
                 This looks like the headless build — install the desktop package to \
                 launch a window, or use `felis sessions …` for scripted access."
            )
        } else {
            anyhow!(
                "unknown felis frontend: no `{bin_stem}` beside felis or on PATH.\n\
                 `felis frontend <name> …` execs `felis-<name>`; install it or check the \
                 spelling."
            )
        }
    } else {
        anyhow::Error::new(err).context(format!("exec {bin_stem}"))
    }
}

/// One session-list line: `<short-id>  <rows>x<cols>  <age>  [title]  [cwd]`.
/// `short_id` is the unique prefix computed against this roster; only the full
/// id is safe to store (docs/reference/cli.md "Machine output").
#[must_use]
fn format_session_line(s: &SessionInfo, short_id: &str) -> String {
    let mut out = format!("{short_id}  {}x{}", s.dims.rows, s.dims.cols);
    if let Some(secs) = s.idle_seconds {
        out.push_str("  ");
        out.push_str(&format_idle_age(secs));
    }
    // A post-exit-grace corpse: attachable for a last look, but the
    // shell is gone, so a picker must not route new work at it.
    if s.exited {
        out.push_str("  (exited)");
    }
    if let Some(fg) = s
        .foreground
        .as_deref()
        .map(str::trim)
        .filter(|f| !f.is_empty())
    {
        out.push_str("  [");
        out.push_str(fg);
        out.push(']');
    }
    if let Some(title) = s.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        out.push_str("  ");
        out.push_str(title);
    }
    if let Some(cwd) = s.cwd.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        out.push_str("  ");
        out.push_str(cwd);
    }
    // `#`-prefixed so an `fzf` query can match `#work` literally.
    for tag in &s.tags {
        out.push_str("  #");
        out.push_str(tag);
    }
    // A full-screen TUI agent emits no OSC 133 prompt marks, so the last
    // notification is the only "blocked / done" status felis can show.
    if let Some(n) = &s.last_notification {
        let text = if n.notification.body.trim().is_empty() {
            n.notification.title.as_deref().unwrap_or("")
        } else {
            n.notification.body.as_str()
        };
        out.push_str("  !");
        out.push_str(&notification_snippet(text));
        out.push_str(" (");
        out.push_str(&format_idle_age(n.age_seconds));
        out.push(')');
    }
    out
}

/// The machine framing carries the untruncated text.
#[must_use]
fn notification_snippet(text: &str) -> String {
    const MAX: usize = 40;
    let oneline: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = oneline.trim();
    if trimmed.chars().count() > MAX {
        let mut s: String = trimmed.chars().take(MAX).collect();
        s.push('…');
        s
    } else {
        trimmed.to_owned()
    }
}

#[must_use]
fn format_idle_age(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 60 * 60 {
        format!("{}m", seconds / 60)
    } else if seconds < 60 * 60 * 24 {
        format!("{}h", seconds / (60 * 60))
    } else {
        format!("{}d", seconds / (60 * 60 * 24))
    }
}

#[cfg(test)]
mod tests;

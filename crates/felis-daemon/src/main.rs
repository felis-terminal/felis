//! `felis-daemon` binary entry point.
//!
//! `felis-daemon serve [--socket PATH]` binds the IPC
//! socket and runs the accept loop until interrupted.

#![cfg_attr(not(test), forbid(unsafe_code))]

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use felis_daemon::{
    ServeError, SessionPool, relay::run_stdio_relay, serve::DaemonCaps, serve_unix,
};
use felis_transport::{
    logging::{self, Console},
    socket::default_socket_path,
};
use tokio::sync::Mutex;
use tracing::info;

#[derive(Debug, Parser)]
#[command(name = "felis-daemon", version = felis_daemon::version())]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Run the IPC server. Runs until the process is killed: there is
    /// no signal handling, and an abrupt death closes the PTY masters,
    /// which SIGHUPs the children (the documented v1 lifecycle posture,
    /// docs/explanation/architecture/session-lifecycle.md).
    Serve {
        /// Override the socket path. When omitted, the endpoint is
        /// `/tmp/felis.<uid>/daemon.sock`, derived from the uid alone
        /// and no environment variable. A path given here must sit in
        /// a directory you own with mode `0700`; the daemon creates
        /// that directory, but nothing above it.
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Raise the daemon's hot-path spans to trace so perf events
        /// fire (`compose_diffs` cycle stats, graphics dispatcher
        /// timing).
        /// Only overrides the default `info,felis_daemon=debug` filter
        /// when `RUST_LOG` is unset; otherwise `RUST_LOG` wins.
        #[arg(long)]
        trace_perf: bool,
        /// The descriptor of the dump an in-place upgrade carried
        /// across `execve`; set only by the predecessor daemon.
        #[cfg(unix)]
        #[arg(long, hide = true)]
        resume_fd: Option<i32>,
    },
    /// Asked by a running daemon before an in-place upgrade: whether
    /// this binary can take over. Answers by exit code.
    #[cfg(unix)]
    #[command(hide = true)]
    UpgradeProbe {
        #[arg(long)]
        dump_version: u32,
        /// The protocol majors the running daemon serves, `MIN-MAX`.
        #[arg(long, value_parser = parse_majors)]
        majors: (u16, u16),
        /// Read the dump from stdin and validate it.
        #[arg(long)]
        check_dump: bool,
    },
    /// Bridge stdin/stdout to the persistent per-UID daemon (the cross-host SSH
    /// relay: `ssh user@host felis-daemon relay`).
    ///
    /// The remote session lives in that persistent daemon (autospawned if
    /// absent) and survives SSH disconnect (docs/reference/ipc.md).
    Relay {
        /// Override the socket path of the daemon to bridge to. Same
        /// default as `serve`, which derives it from the uid, so a login
        /// that exports no runtime variable still reaches this host's
        /// daemon.
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Fail when the persistent daemon is not up instead of
        /// autospawning it: the headless verbs' no-silent-resurrection
        /// posture, carried across the SSH hop by the client appending
        /// this flag for read/drive dials.
        #[arg(long)]
        no_spawn: bool,
    },
}

fn build_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve {
            socket,
            trace_perf,
            #[cfg(unix)]
            resume_fd,
        } => {
            // Only `serve` tees into the log file: a relay teeing into the same
            // file would race the serving daemon's rotation.
            logging::init(
                Console::Stderr,
                Some("daemon.log"),
                log_filter_directive(trace_perf),
            );
            #[cfg(unix)]
            if let Some(resume_fd) = resume_fd {
                return build_runtime()?.block_on(run_resumed(socket, resume_fd));
            }
            build_runtime()?.block_on(run_serve_unix(socket))
        }
        #[cfg(unix)]
        Cmd::UpgradeProbe {
            dump_version,
            majors,
            check_dump,
        } => std::process::exit(felis_daemon::upgrade::probe::answer(
            dump_version,
            majors,
            check_dump,
        )),
        Cmd::Relay { socket, no_spawn } => {
            logging::init(Console::Stderr, None, log_filter_directive(false));
            let socket = socket_or_default(socket)?;
            build_runtime()?
                .block_on(run_stdio_relay(socket, no_spawn))
                .context("run stdio relay")
        }
    }
}

fn socket_or_default(socket: Option<PathBuf>) -> Result<PathBuf> {
    match socket {
        Some(path) => Ok(path),
        None => default_socket_path().context("resolve socket path"),
    }
}

async fn run_serve_unix(socket_override: Option<PathBuf>) -> Result<()> {
    let socket = socket_or_default(socket_override)?;
    info!(path = %socket.display(), "starting felis-daemon");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    serve_outcome(serve_unix(&socket, daemon_caps(), pool).await)
}

#[cfg(unix)]
async fn run_resumed(socket_override: Option<PathBuf>, resume_fd: i32) -> Result<()> {
    let socket = socket_or_default(socket_override)?;
    info!(path = %socket.display(), "felis-daemon taking over after an upgrade");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let caps = daemon_caps();
    let listener = felis_daemon::upgrade::restore::restore(resume_fd, &socket, &pool, caps.idle)
        .await
        .context("restore after an in-place upgrade")?;
    serve_outcome(
        felis_daemon::serve::serve_resumed(
            listener,
            felis_transport::Endpoint::unix(socket),
            caps,
            pool,
            felis_daemon::serve::default_session_factory(),
        )
        .await,
    )
}

fn daemon_caps() -> DaemonCaps {
    DaemonCaps {
        upgrade: felis_daemon::upgrade::UpgradeState::replaceable(),
        ..DaemonCaps::default()
    }
}

#[cfg(unix)]
fn parse_majors(text: &str) -> Result<(u16, u16), String> {
    let (min, max) = text.split_once('-').ok_or("expected MIN-MAX")?;
    let min = min.parse().map_err(|err| format!("{err}"))?;
    let max = max.parse().map_err(|err| format!("{err}"))?;
    if min > max {
        return Err("MIN exceeds MAX".to_owned());
    }
    Ok((min, max))
}

fn serve_outcome(outcome: Result<(), ServeError>) -> Result<()> {
    match outcome {
        Ok(()) => Ok(()),
        Err(ServeError::Bind(e)) => Err(anyhow::Error::new(e).context("bind daemon socket")),
        Err(ServeError::Accept(e)) => Err(anyhow::Error::new(e).context("accept connection")),
        Err(ServeError::Io(e)) => Err(anyhow::Error::new(e).context("daemon io error")),
        Err(e @ ServeError::Endpoint(_)) => {
            Err(anyhow::Error::new(e).context("resolve daemon endpoint"))
        }
    }
}

/// The filter directive used when `RUST_LOG` is unset.
const fn log_filter_directive(trace_perf: bool) -> &'static str {
    if trace_perf {
        "info,felis_daemon=debug,felis_daemon::serve=trace,felis_daemon::graphics=trace"
    } else {
        "info,felis_daemon=debug"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// `--trace-perf` parses on `serve` and defaults to `false`.
    #[test]
    fn cli_parses_trace_perf_flag() {
        let cli = Cli::try_parse_from(["felis-daemon", "serve", "--trace-perf"])
            .expect("parses with --trace-perf");
        let Cmd::Serve { trace_perf, .. } = cli.cmd else {
            panic!("expected Serve, got {:?}", cli.cmd);
        };
        assert!(trace_perf);
        let default = Cli::try_parse_from(["felis-daemon", "serve"]).expect("parses bare serve");
        let Cmd::Serve { trace_perf, .. } = default.cmd else {
            panic!("expected Serve, got {:?}", default.cmd);
        };
        assert!(!trace_perf);
    }

    /// `log_filter_directive(true)` widens hot-path targets to trace
    /// without bumping every other crate.
    #[test]
    fn perf_trace_directive_includes_hot_path_targets() {
        let perf = log_filter_directive(true);
        assert!(perf.contains("felis_daemon::serve=trace"));
        assert!(perf.contains("felis_daemon::graphics=trace"));
        let normal = log_filter_directive(false);
        assert!(
            !normal.contains("=trace"),
            "default filter must not include trace: {normal}"
        );
        assert!(normal.contains("felis_daemon=debug"));
    }
}

//! Logging bootstrap, log-file, and panic-capture plumbing for the felis
//! binaries. GUI launches (`felis.app`, autospawn from a bundled client)
//! inherit `/dev/null` for stderr, so logs and panics also tee into a
//! per-binary file.

use std::fs::File;
use std::path::PathBuf;

/// Size-based rather than start-time rotation: several client processes
/// share one `client.log`, and a crash-looping daemon would erase the
/// previous run's evidence.
const ROTATE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Console {
    /// The daemon's `relay` subcommand carries the IPC wire on stdout,
    /// where a stray log line would corrupt frames.
    Stderr,
    Stdout,
}

pub fn init(console: Console, log_file_name: Option<&str>, fallback_directive: &str) {
    use tracing_subscriber::fmt::writer::{BoxMakeWriter, MakeWriterExt as _};

    let ansi = match console {
        Console::Stderr => std::io::IsTerminal::is_terminal(&std::io::stderr()),
        Console::Stdout => std::io::IsTerminal::is_terminal(&std::io::stdout()),
    };
    let file = log_file_name.and_then(open_log_file);
    let (writer, log_path) = match (console, file) {
        (Console::Stderr, Some((file, path))) => (
            BoxMakeWriter::new(std::io::stderr.and(std::sync::Mutex::new(file))),
            Some(path),
        ),
        (Console::Stderr, None) => (BoxMakeWriter::new(std::io::stderr), None),
        (Console::Stdout, Some((file, path))) => (
            BoxMakeWriter::new(std::io::stdout.and(std::sync::Mutex::new(file))),
            Some(path),
        ),
        (Console::Stdout, None) => (BoxMakeWriter::new(std::io::stdout), None),
    };
    tracing_subscriber::fmt()
        .with_writer(writer)
        // The default (`true`) `eprintln!`s on a sink write failure; a
        // detached daemon's stderr can be a dead pipe, so that write fails
        // inside the panic hook and aborts the process.
        .log_internal_errors(false)
        .with_ansi(ansi)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(fallback_directive)),
        )
        .init();
    install_panic_hook();
    if let Some(path) = log_path {
        tracing::info!(path = %path.display(), "teeing logs to file");
    }
}

/// macOS gets `~/Library/Logs/felis` rather than `TMPDIR`: the periodic
/// cleaner there would drop crash evidence before anyone reads it.
#[must_use]
pub fn log_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        // `BaseDirs` rather than a bare `$HOME` read: an autospawned daemon
        // can inherit an environment without it, and `BaseDirs` falls back
        // to `getpwuid_r`.
        let home = directories::BaseDirs::new()?;
        return Some(home.home_dir().join("Library/Logs/felis"));
    }
    let dirs = directories::ProjectDirs::from("", "", "felis")?;
    Some(
        dirs.state_dir()
            .unwrap_or_else(|| dirs.data_local_dir())
            .to_path_buf(),
    )
}

/// `None` means console-only logging: a missing HOME or unwritable
/// directory must never take the binary down.
#[must_use]
pub fn open_log_file(name: &str) -> Option<(File, PathBuf)> {
    let dir = log_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(name);
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > ROTATE_BYTES) {
        drop(std::fs::rename(&path, dir.join(format!("{name}.old"))));
    }
    let file = File::options().create(true).append(true).open(&path).ok()?;
    Some((file, path))
}

pub fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        // A panic inside the hook (a dead stderr sink, a writer mutex
        // poisoned by the panic being recorded) would abort the process;
        // swallow it so the original still reaches `prev`.
        drop(std::panic::catch_unwind(std::panic::AssertUnwindSafe(
            || {
                tracing::error!(target: "panic", %info, "panic\n{backtrace}");
            },
        )));
        prev(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_dir_is_per_user_and_ends_in_felis() {
        // Rotation renames must stay inside felis's own directory.
        let dir = log_dir().expect("a home directory exists in the test environment");
        // On Windows `directories` puts the local data dir at `...\felis\data`.
        assert!(
            dir.ends_with("felis") || dir.ends_with("felis/data"),
            "{}",
            dir.display()
        );
    }
}

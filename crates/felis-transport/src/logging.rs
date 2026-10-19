//! Logging bootstrap, log-file, and panic-capture plumbing for the felis
//! binaries. GUI launches (`felis.app`, autospawn from a bundled client)
//! inherit `/dev/null` for stderr, so logs and panics also tee into a
//! per-binary file.

use std::fs::File;
use std::path::{Path, PathBuf};

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

/// The tee's file name, and the socket of the daemon it belongs to: a
/// non-default endpoint logs beside its socket ([`adjacent_log_dir`]).
#[derive(Debug, Clone, Copy)]
pub struct LogFile<'a> {
    pub name: &'a str,
    pub socket: Option<&'a Path>,
}

pub fn init(console: Console, log_file: Option<LogFile<'_>>, fallback_directive: &str) {
    use tracing_subscriber::fmt::writer::{BoxMakeWriter, MakeWriterExt as _};

    let ansi = match console {
        Console::Stderr => std::io::IsTerminal::is_terminal(&std::io::stderr()),
        Console::Stdout => std::io::IsTerminal::is_terminal(&std::io::stdout()),
    };
    let file = log_file.and_then(|log| open_log_file(log.name, log.socket));
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

/// Where a daemon on `socket` logs when that directory qualifies: the
/// socket's parent for any endpoint but the default, so test and
/// experiment daemons stay out of the user's own `daemon.log`. `None`
/// for the default endpoint and on Windows, whose pipe names have no
/// directory.
#[cfg(unix)]
#[must_use]
pub fn adjacent_log_dir(socket: &Path) -> Option<PathBuf> {
    if crate::socket::default_socket_path().is_ok_and(|default| default == socket) {
        return None;
    }
    Some(crate::unix::socket_parent(socket))
}

#[cfg(not(unix))]
#[must_use]
pub const fn adjacent_log_dir(_socket: &Path) -> Option<PathBuf> {
    None
}

/// `None` means console-only logging: a missing HOME or unwritable
/// directory must never take the binary down.
#[must_use]
pub fn open_log_file(name: &str, socket: Option<&Path>) -> Option<(File, PathBuf)> {
    #[cfg(unix)]
    if let Some(opened) = socket
        .and_then(adjacent_log_dir)
        .and_then(|dir| open_beside(&dir, name))
    {
        return Some(opened);
    }
    #[cfg(not(unix))]
    let _ = socket;
    let dir = log_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(name);
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > ROTATE_BYTES) {
        drop(std::fs::rename(&path, dir.join(format!("{name}.old"))));
    }
    let file = File::options().create(true).append(true).open(&path).ok()?;
    Some((file, path))
}

/// Only into a directory the listener would bind in, judged and written
/// through one descriptor, so a rename of the path after the judge
/// cannot redirect the file. Never creates the directory: the listener
/// creates a missing one `0700`, which a `create_dir_all` under umask
/// `022` would preempt with `0755`.
#[cfg(unix)]
fn open_beside(dir: &Path, name: &str) -> Option<(File, PathBuf)> {
    use rustix::fs::{AtFlags, Mode, OFlags};
    use std::os::unix::fs::MetadataExt as _;

    let dirfd = File::from(
        rustix::fs::open(
            dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .ok()?,
    );
    let facts = dirfd.metadata().ok()?;
    crate::unix::judge_socket_dir(
        dir,
        rustix::process::geteuid().as_raw(),
        crate::unix::DirFacts {
            uid: facts.uid(),
            mode: facts.mode() & 0o777,
        },
    )
    .ok()?;
    if rustix::fs::statat(&dirfd, name, AtFlags::SYMLINK_NOFOLLOW)
        .is_ok_and(|stat| u64::try_from(stat.st_size).is_ok_and(|len| len > ROTATE_BYTES))
    {
        rustix::fs::renameat(&dirfd, name, &dirfd, format!("{name}.old")).ok();
    }
    let file = rustix::fs::openat(
        &dirfd,
        name,
        OFlags::WRONLY | OFlags::APPEND | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .ok()?;
    Some((File::from(file), dir.join(name)))
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

    #[cfg(unix)]
    mod beside_the_socket {
        use super::super::*;
        use std::fs;
        use std::os::unix::fs::PermissionsExt as _;

        fn private_dir() -> tempfile::TempDir {
            let dir = tempfile::tempdir().unwrap();
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
            dir
        }

        #[test]
        fn the_default_endpoint_has_no_adjacent_log_dir() {
            let default = crate::socket::default_socket_path().unwrap();
            assert_eq!(adjacent_log_dir(&default), None);
        }

        #[test]
        fn another_endpoint_logs_in_its_socket_directory() {
            let dir = private_dir();
            let socket = dir.path().join("d.sock");

            let (_file, path) = open_log_file("daemon.log", Some(&socket)).unwrap();

            assert_eq!(path, dir.path().join("daemon.log"));
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        #[test]
        fn a_missing_socket_directory_is_not_created_for_the_log() {
            let dir = private_dir();
            let missing = dir.path().join("not-yet");

            assert!(open_beside(&missing, "daemon.log").is_none());
            assert!(
                !missing.exists(),
                "the listener creates it 0700, not the log"
            );
        }

        #[test]
        fn a_directory_others_can_read_is_not_a_log_destination() {
            let dir = private_dir();
            let loose = dir.path().join("loose");
            fs::create_dir(&loose).unwrap();
            fs::set_permissions(&loose, fs::Permissions::from_mode(0o755)).unwrap();

            assert!(open_beside(&loose, "daemon.log").is_none());
            assert!(!loose.join("daemon.log").exists());
        }

        #[test]
        fn a_symlinked_socket_directory_is_not_a_log_destination() {
            let dir = private_dir();
            let real = dir.path().join("real");
            fs::create_dir(&real).unwrap();
            fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap();

            assert!(open_beside(&link, "daemon.log").is_none());
            assert!(!real.join("daemon.log").exists());
        }

        #[test]
        fn a_planted_log_symlink_is_not_followed() {
            let dir = private_dir();
            let target = dir.path().join("elsewhere");
            std::os::unix::fs::symlink(&target, dir.path().join("daemon.log")).unwrap();

            assert!(open_beside(dir.path(), "daemon.log").is_none());
            assert!(!target.exists());
        }

        #[test]
        fn an_oversized_adjacent_log_rotates_to_old() {
            let dir = private_dir();
            let log = dir.path().join("daemon.log");
            let big = File::create(&log).unwrap();
            big.set_len(ROTATE_BYTES + 1).unwrap();

            let (_file, path) = open_beside(dir.path(), "daemon.log").unwrap();

            assert_eq!(fs::metadata(&path).unwrap().len(), 0);
            assert_eq!(
                fs::metadata(dir.path().join("daemon.log.old"))
                    .unwrap()
                    .len(),
                ROTATE_BYTES + 1
            );
        }
    }

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

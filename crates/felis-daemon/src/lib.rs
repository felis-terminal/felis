//! `felis-daemon`: session pool, IPC server, signal handling.
//!
//! The library half exposes the daemon's plumbing so an embedder (a test
//! harness or a future cross-process spawner) can drive it without the
//! binary's CLI.

// `deny`, not `forbid`: `forbid` cannot be relaxed at the audited
// `#[allow(unsafe_code)]` sites (`graphics::image_decode`'s shm
// reader, `foreground`'s `proc_pidpath` lookup).
#![cfg_attr(not(test), deny(unsafe_code))]

use std::env;
use std::sync::Arc;

use felis_protocol::SessionHex;
use felis_pty::{ChildHandle, Command, PtyError, PtyReader, PtyWriter, Resizer, Size};

use crate::parse_sink::ParseSignals;
use crate::pool::ParseCore;

pub mod agent;
pub mod child_env;
pub mod foreground;
pub mod graphics;
pub(crate) mod locale;
#[cfg(target_os = "linux")]
pub mod notify;
pub mod parse_sink;
pub mod pool;
pub mod relay;
pub mod serve;
pub(crate) mod terminfo;
pub mod upgrade;
pub use pool::{DEFAULT_COLS, DEFAULT_ROWS, SessionId, SessionMeta, SessionPool};
pub use serve::{ServeError, serve_unix};

/// Initial PTY size before the client reports its viewport.
pub const DEFAULT_SIZE: Size = Size {
    rows: 24,
    cols: 80,
    pixel_width: 0,
    pixel_height: 0,
};

/// The canonical identity line for `--version`
/// (`docs/reference/workspace.md` "Versioning"). The revision lives
/// here, not in `TERM_PROGRAM_VERSION`: programs inside the terminal
/// want stable semver, and compatibility is gated by the preface's
/// protocol major/minor, so the revision is free to churn.
#[must_use]
pub const fn version() -> &'static str {
    concat!(
        env!("CARGO_PKG_VERSION"),
        " (",
        env!("FELIS_BUILD_STAMP"),
        ")"
    )
}

/// The build the daemon was compiled from (`build.rs`), typed. Sent in
/// `Welcome` so `felis version` shows which build the running daemon
/// is, as distinct from the on-disk binary.
#[must_use]
pub fn build_identity() -> felis_protocol::BuildIdentity {
    felis_protocol::BuildIdentity::from_build_env(
        env!("CARGO_PKG_VERSION"),
        env!("FELIS_BUILD_STAMP"),
    )
}

pub type SessionError = PtyError;

/// Env-var denylist applied to every child shell
/// (`docs/explanation/security-model.md`).
///
/// Scrubs single-use startup tokens, conflicting emulator flags,
/// and felis terminal-identity hatch variables.
const ENV_DENYLIST: &[&str] = &[
    "XDG_ACTIVATION_TOKEN",
    NOTIFY_SOCKET_ENV,
    "DESKTOP_STARTUP_ID",
    "VTE_VERSION",
    "FELIS_TERM",
    "FELIS_TERM_PROGRAM",
];

/// Keys forbidden in `SpawnArgs.env` (REQ-912) and scrubbed from inherited base.
///
/// Ensures explicit config validation and base sanitization agree, while
/// allowing unreserved identity stamps (`TERM`, etc.) to be overridden.
#[must_use]
pub const fn reserved_env_keys() -> &'static [&'static str] {
    &RESERVED_ENV_KEYS
}

/// Derived from [`ENV_DENYLIST`] rather than restated beside it, so the
/// two cannot drift.
const RESERVED_ENV_KEYS: [&str; ENV_DENYLIST.len() + ADDRESSING_ENV.len()] =
    reserved_env_keys_from_denylist();

/// Stamped unconditionally and reserved: a value the child did not get
/// from felis names something other than where it is running.
const ADDRESSING_ENV: [&str; 2] = ["FELIS_SESSION_ID", SOCKET_ENV];

const fn reserved_env_keys_from_denylist()
-> [&'static str; ENV_DENYLIST.len() + ADDRESSING_ENV.len()] {
    let mut keys = ["FELIS_SESSION_ID"; ENV_DENYLIST.len() + ADDRESSING_ENV.len()];
    let mut at = 0;
    while at < ADDRESSING_ENV.len() {
        keys[at] = ADDRESSING_ENV[at];
        at += 1;
    }
    at = 0;
    while at < ENV_DENYLIST.len() {
        keys[at + ADDRESSING_ENV.len()] = ENV_DENYLIST[at];
        at += 1;
    }
    keys
}

/// The systemd user manager's readiness endpoint for a `Type=notify`
/// service. A child that inherited it could report this daemon ready,
/// or stopped, in its place.
pub(crate) const NOTIFY_SOCKET_ENV: &str = "NOTIFY_SOCKET";

/// The endpoint this daemon serves; the client half of the contract is
/// `felis_client_core::local_socket` (docs/reference/terminal-identity.md).
const SOCKET_ENV: &str = "FELIS_SOCKET";

/// Must match the entry name in `share/terminfo/felis.terminfo`
/// (docs/reference/terminal-identity.md).
const DEFAULT_TERM: &str = "xterm-felis";

const DEFAULT_TERM_PROGRAM: &str = "felis";

/// The owned halves of a freshly spawned PTY session plus the parse
/// pipeline the PTY parse thread feeds; [`pool::Session`] wraps these
/// with the async-side state.
pub struct SpawnedPty {
    /// Carries only the EOF/error lifecycle signal; the sink on the parse
    /// thread consumes the data (`felis_pty::ByteSink`).
    pub reader: PtyReader,
    pub writer: PtyWriter,
    pub child: ChildHandle,
    pub resizer: Resizer,
    pub core: Arc<parking_lot::Mutex<ParseCore>>,
    pub signals: Arc<ParseSignals>,
    /// Parks the PTY threads for an in-place upgrade.
    pub quiescer: felis_pty::Quiescer,
}

impl SpawnedPty {
    /// Default shell command: `$SHELL`, falling back to `/bin/sh`.
    ///
    /// Reads `$SHELL` from `base` before daemon environment, since a daemon's
    /// own environment may reflect an auto-spawning login or relay host.
    #[must_use]
    pub fn default_shell_command(base: Option<&[child_env::EnvEntry]>) -> Command {
        let shell = base
            .and_then(|entries| child_env::lookup(entries, "SHELL", child_env::TARGET_IS_WINDOWS))
            .and_then(felis_pty::env_from_bytes)
            .or_else(|| env::var_os("SHELL"))
            .unwrap_or_else(|| std::ffi::OsString::from(default_shell()));
        #[cfg_attr(windows, expect(unused_mut))]
        let mut cmd = Command::new(&shell);
        // `-l` so login-shell initialization runs. Unix-only: powershell.exe
        // has no login-shell flag and would treat `-l` as a command.
        #[cfg(unix)]
        cmd.arg("-l");
        cmd
    }

    pub fn spawn(command: Command) -> Result<Self, SessionError> {
        let mut parse_core = ParseCore::new(DEFAULT_ROWS, DEFAULT_COLS);
        if let Some(term) = command
            .get_envs()
            .find(|(key, _)| *key == "TERM")
            .and_then(|(_, value)| value.to_str())
        {
            parse_core.grid.set_term_name(term);
        }
        let core = Arc::new(parking_lot::Mutex::new(parse_core));
        let signals = Arc::new(ParseSignals::new());
        let sink = parse_sink::build_sink(Arc::clone(&core), Arc::clone(&signals));
        let session = felis_pty::spawn(command, DEFAULT_SIZE, sink)?;
        Ok(Self::from_session(session, core, signals))
    }

    /// Resumes a session an in-place upgrade carried across `execve`: the
    /// PTY master and the child are already this process's, and `core`
    /// is the terminal state the predecessor left, installed before the
    /// parse thread reads a byte.
    #[cfg(unix)]
    pub fn adopt(
        master: std::os::fd::OwnedFd,
        pid: i32,
        exit_status: Option<i32>,
        core: ParseCore,
    ) -> Result<Self, SessionError> {
        let core = Arc::new(parking_lot::Mutex::new(core));
        let signals = Arc::new(ParseSignals::new());
        let sink = parse_sink::build_sink(Arc::clone(&core), Arc::clone(&signals));
        let session = felis_pty::adopt(master, pid, exit_status, sink)?;
        Ok(Self::from_session(session, core, signals))
    }

    fn from_session(
        session: felis_pty::PtySession,
        core: Arc<parking_lot::Mutex<ParseCore>>,
        signals: Arc<ParseSignals>,
    ) -> Self {
        let quiescer = session.quiescer();
        let (reader, writer, child, resizer) = session.split();
        Self {
            reader,
            writer,
            child,
            resizer,
            core,
            signals,
            quiescer,
        }
    }
}

/// Apply child-env policy (inherit + denylist + forced overrides) to a [`Command`].
///
/// Honors terminal identity hatches (`docs/reference/terminal-identity.md`),
/// sets session addressing variables (`FELIS_SESSION_ID`, `FELIS_SOCKET`),
/// and clears stale daemon socket inherited from outer sessions.
pub fn apply_env_policy(
    cmd: &mut Command,
    session_id: u128,
    hatch: &TermHatch,
    endpoint: Option<&std::ffi::OsStr>,
    lang: Option<&str>,
) {
    for &key in ENV_DENYLIST {
        cmd.env_remove(key);
    }
    let (term, term_program) =
        resolve_term_identity(hatch.term.clone(), hatch.term_program.clone());
    cmd.env("TERM", term);
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TERM_PROGRAM", term_program);
    cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
    cmd.env("FELIS_SESSION_ID", SessionHex(session_id).to_string());
    match endpoint {
        Some(endpoint) => cmd.env(SOCKET_ENV, endpoint),
        None => cmd.env_remove(SOCKET_ENV),
    };
    if let Some(lang) = lang {
        cmd.env("LANG", lang);
    }
}

/// The identity escape hatch (`FELIS_TERM` / `FELIS_TERM_PROGRAM`) as
/// the base a create resolved spells it, not the daemon's live
/// environment (a warm daemon's copy came from an earlier login, or
/// another host). Read before the denylist scrub removes the two keys
/// (docs/reference/terminal-identity.md).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TermHatch {
    pub term: Option<String>,
    pub term_program: Option<String>,
}

impl TermHatch {
    /// The hatch as the daemon's own environment spells it: the fallback
    /// for every entry a base leaves unset.
    #[must_use]
    pub fn from_daemon_env() -> Self {
        Self {
            term: env::var("FELIS_TERM").ok(),
            term_program: env::var("FELIS_TERM_PROGRAM").ok(),
        }
    }

    /// The hatch as a base snapshot spells it. An undecodable value is
    /// left unset rather than lossily converted: falling back beats
    /// stamping mojibake into `TERM`.
    #[must_use]
    pub fn from_base(entries: &[child_env::EnvEntry], windows: bool) -> Self {
        let read = |key| {
            child_env::lookup(entries, key, windows)
                .and_then(felis_pty::env_from_bytes)
                .and_then(|value| value.into_string().ok())
        };
        Self {
            term: read("FELIS_TERM"),
            term_program: read("FELIS_TERM_PROGRAM"),
        }
    }

    /// Per variable, not whole-hatch: a base that sets only `FELIS_TERM`
    /// leaves `FELIS_TERM_PROGRAM` to the fallback.
    #[must_use]
    pub fn or(self, fallback: Self) -> Self {
        Self {
            term: self.term.or(fallback.term),
            term_program: self.term_program.or(fallback.term_program),
        }
    }
}

/// Pure so the precedence is testable without `set_var`, which is
/// `unsafe` and denied crate-wide.
fn resolve_term_identity(
    term_override: Option<String>,
    term_program_override: Option<String>,
) -> (String, String) {
    (
        term_override.unwrap_or_else(|| DEFAULT_TERM.to_string()),
        term_program_override.unwrap_or_else(|| DEFAULT_TERM_PROGRAM.to_string()),
    )
}

#[cfg(unix)]
const fn default_shell() -> &'static str {
    "/bin/sh"
}

#[cfg(windows)]
const fn default_shell() -> &'static str {
    "powershell.exe"
}

/// The `PATH` this crate's shell fixtures hand their children. The FHS
/// pair keeps first claim; the runner's own `PATH` is the tail that
/// lets a host without an FHS `/bin` resolve anything (NixOS links only
/// `sh` there). Without it `sh` runs every fixture body with its
/// externals missing and reports no failure.
#[cfg(all(test, unix))]
pub(crate) fn fixture_path() -> std::ffi::OsString {
    let mut path = std::ffi::OsString::from("/bin:/usr/bin");
    if let Some(host) = env::var_os("PATH") {
        path.push(":");
        path.push(host);
    }
    path
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use felis_pty::Command;
    use tokio::io::AsyncReadExt;

    #[test]
    fn default_term_matches_the_shipped_terminfo_entry() {
        // Renaming either side alone leaves every child with a TERM no
        // terminfo database resolves.
        let source = include_str!("../../../share/terminfo/felis.terminfo");
        let entry = format!("{DEFAULT_TERM}|");
        assert!(
            source.lines().any(|line| line.starts_with(&entry)),
            "share/terminfo/felis.terminfo declares no `{entry}...` entry",
        );
    }

    #[cfg(windows)]
    fn comspec() -> std::ffi::OsString {
        env::var_os("ComSpec").unwrap_or_else(|| r"C:\Windows\System32\cmd.exe".into())
    }

    #[cfg(unix)]
    fn env_dump_command() -> Command {
        Command::new("/usr/bin/env")
    }

    #[cfg(windows)]
    fn env_dump_command() -> Command {
        let mut cmd = Command::new(comspec());
        cmd.args(["/c", "set"]);
        cmd
    }

    #[cfg(unix)]
    fn echo_marker_command(marker: &str) -> Command {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", &format!("printf '{marker}\\n'; exit 0")]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    }

    #[cfg(windows)]
    fn echo_marker_command(marker: &str) -> Command {
        let mut cmd = Command::new(comspec());
        cmd.args(["/c", &format!("echo {marker}")]);
        cmd
    }

    /// Tests read text from the grid: in sink mode the reader half carries
    /// only EOF.
    fn grid_text(core: &Arc<parking_lot::Mutex<ParseCore>>) -> String {
        let guard = core.lock();
        let grid = &guard.grid;
        let clusters = grid.cluster_table();
        let mut out = String::new();
        for (row, _) in grid.rows_with_wrap(felis_grid::AltScreenRows::Include) {
            out.push_str(&felis_grid::row_text_trim(row, clusters));
            out.push('\n');
        }
        out
    }

    async fn read_child_env(endpoint: Option<&str>, setup: impl FnOnce(&mut Command)) -> String {
        read_child_env_with_lang(endpoint, None, setup).await
    }

    async fn read_child_env_with_lang(
        endpoint: Option<&str>,
        lang: Option<&str>,
        setup: impl FnOnce(&mut Command),
    ) -> String {
        let mut cmd = env_dump_command();
        setup(&mut cmd);
        apply_env_policy(
            &mut cmd,
            0xfe115,
            &TermHatch::from_daemon_env(),
            endpoint.map(std::ffi::OsStr::new),
            lang,
        );
        let core =
            read_until_output_complete(SpawnedPty::spawn(cmd).expect("spawn env dumper")).await;
        grid_text(&core)
    }

    /// Drive the reader to EOF, waiting for all parse sink calls to finish.
    ///
    /// Keeps `resizer` alive to prevent early `ClosePseudoConsole` on Windows
    /// before child processes have echoed output.
    async fn read_until_output_complete(pty: SpawnedPty) -> Arc<parking_lot::Mutex<ParseCore>> {
        let SpawnedPty {
            mut reader,
            core,
            child,
            resizer,
            ..
        } = pty;
        #[cfg(windows)]
        {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while child.try_wait().expect("try_wait").is_none() {
                assert!(tokio::time::Instant::now() < deadline, "child never exited");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            drop(resizer);
        }
        let mut buf = Vec::new();
        let done =
            tokio::time::timeout(Duration::from_secs(10), reader.read_to_end(&mut buf)).await;
        drop(done.expect("child never reached EOF"));
        #[cfg(unix)]
        drop(resizer);
        drop(child);
        core
    }

    async fn spawned_term_name(hatch: &TermHatch) -> Option<String> {
        let mut cmd = env_dump_command();
        cmd.env("TERM", "wrong-term");
        apply_env_policy(&mut cmd, 0xfe115, hatch, None, None);
        let core =
            read_until_output_complete(SpawnedPty::spawn(cmd).expect("spawn env dumper")).await;
        core.lock().grid.term_name().map(str::to_owned)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_grid_answers_xtgettcap_tn_with_the_stamped_term() {
        assert_eq!(
            spawned_term_name(&TermHatch::default()).await.as_deref(),
            Some("xterm-felis"),
        );
        let hatch = TermHatch {
            term: Some("xterm-kitty".to_owned()),
            term_program: None,
        };
        assert_eq!(
            spawned_term_name(&hatch).await.as_deref(),
            Some("xterm-kitty"),
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_env_policy_inherits_parent_env_by_default() {
        let marker = "felis-inherit-test-marker-value";
        let text = read_child_env(None, |cmd| {
            cmd.env("FELIS_INHERIT_TEST", marker);
        })
        .await;
        assert!(
            text.contains(&format!("FELIS_INHERIT_TEST={marker}")),
            "inherited env did not reach child; got: {text}",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_env_policy_strips_denylist_entries() {
        let text = read_child_env(None, |cmd| {
            cmd.env("XDG_ACTIVATION_TOKEN", "must-not-leak");
            cmd.env("DESKTOP_STARTUP_ID", "must-not-leak");
            cmd.env("VTE_VERSION", "must-not-leak");
            cmd.env("NOTIFY_SOCKET", "must-not-leak");
        })
        .await;
        assert!(!text.contains("XDG_ACTIVATION_TOKEN"), "got: {text}");
        assert!(!text.contains("DESKTOP_STARTUP_ID"), "got: {text}");
        assert!(!text.contains("VTE_VERSION"), "got: {text}");
        assert!(!text.contains("NOTIFY_SOCKET"), "got: {text}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_env_policy_overrides_term_identifiers_even_when_inherited() {
        let text = read_child_env(None, |cmd| {
            cmd.env("TERM", "wrong-term");
            cmd.env("TERM_PROGRAM", "wrong-program");
            cmd.env("TERM_PROGRAM_VERSION", "0.0.0-wrong");
        })
        .await;
        assert!(
            text.contains("TERM=xterm-felis"),
            "TERM override missing or shadowed; got: {text}",
        );
        assert!(
            text.contains("COLORTERM=truecolor"),
            "COLORTERM override missing or shadowed; got: {text}",
        );
        assert!(
            text.contains("TERM_PROGRAM=felis"),
            "TERM_PROGRAM override missing or shadowed; got: {text}",
        );
        assert!(
            text.contains(&format!(
                "TERM_PROGRAM_VERSION={}",
                env!("CARGO_PKG_VERSION")
            )),
            "TERM_PROGRAM_VERSION override missing or shadowed; got: {text}",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_env_policy_stamps_session_id_overriding_inherited() {
        let text = read_child_env(None, |cmd| {
            cmd.env("FELIS_SESSION_ID", "stale-inherited-id");
        })
        .await;
        assert!(
            text.contains(&format!("FELIS_SESSION_ID={:032x}", 0xfe115)),
            "session id stamp missing or shadowed; got: {text}",
        );
    }

    /// A daemon auto-spawned from inside another felis window inherits
    /// that one's address.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_env_policy_stamps_the_endpoint_overriding_inherited() {
        let text = read_child_env(Some("/run/felis-work/daemon.sock"), |cmd| {
            cmd.env("FELIS_SOCKET", "/run/felis-someone-elses/daemon.sock");
        })
        .await;
        assert!(
            text.contains("FELIS_SOCKET=/run/felis-work/daemon.sock"),
            "endpoint stamp missing or shadowed; got: {text}",
        );
        assert!(
            !text.contains("felis-someone-elses"),
            "the inherited address survived the stamp; got: {text}",
        );
    }

    /// An inherited address is removed, not passed on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unnamed_endpoint_scrubs_an_inherited_address() {
        let text = read_child_env(None, |cmd| {
            cmd.env("FELIS_SOCKET", "/run/felis-someone-elses/daemon.sock");
        })
        .await;
        assert!(!text.contains("FELIS_SOCKET"), "got: {text}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_resolved_lang_reaches_the_child() {
        let text = read_child_env_with_lang(None, Some("xx_YY.UTF-8"), |cmd| {
            cmd.env_remove("LANG");
        })
        .await;
        assert!(
            text.contains("LANG=xx_YY.UTF-8"),
            "LANG fill missing; got: {text}",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_resolved_lang_leaves_an_inherited_one_alone() {
        let text = read_child_env_with_lang(None, None, |cmd| {
            cmd.env("LANG", "ja_JP.UTF-8");
        })
        .await;
        assert!(
            text.contains("LANG=ja_JP.UTF-8"),
            "the inherited LANG did not survive; got: {text}",
        );
    }

    /// Asserted with `pwd` (`getcwd(3)`): the inherited `PWD` still names
    /// the daemon's directory, so an env check would pass with the chdir
    /// dropped.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_starts_the_child_in_the_requested_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        // macOS puts temp dirs under the `/var` → `/private/var` symlink and
        // `pwd` reports the resolved path.
        let resolved = dir.path().canonicalize().expect("canonicalize tempdir");

        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "pwd"]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd.cwd(dir.path());
        let core = read_until_output_complete(SpawnedPty::spawn(cmd).expect("spawn pwd")).await;

        let text = grid_text(&core);
        assert!(
            text.contains(&resolved.to_string_lossy().into_owned()),
            "child did not start in the requested cwd {}; got: {text}",
            resolved.display(),
        );
    }

    #[test]
    fn term_identity_defaults_to_honest_felis_when_no_override() {
        let (term, program) = resolve_term_identity(None, None);
        assert_eq!(term, "xterm-felis");
        assert_eq!(program, "felis");
    }

    #[test]
    fn term_identity_escape_hatch_overrides_each_field_independently() {
        let (term, program) =
            resolve_term_identity(Some("xterm-kitty".to_string()), Some("kitty".to_string()));
        assert_eq!(term, "xterm-kitty");
        assert_eq!(program, "kitty");

        let (term, program) = resolve_term_identity(Some("xterm-kitty".to_string()), None);
        assert_eq!(term, "xterm-kitty");
        assert_eq!(
            program, "felis",
            "unset program must keep the honest default"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawns_explicit_command_and_parses_its_output() {
        let core = read_until_output_complete(
            SpawnedPty::spawn(echo_marker_command("session-up")).expect("spawn"),
        )
        .await;

        let text = grid_text(&core);
        assert!(
            text.contains("session-up"),
            "the marker never reached the grid; got: {text:?}"
        );
    }
}

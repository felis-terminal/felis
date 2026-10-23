//! Spawns transient sessions for piped regions on the client machine
//! (`docs/explanation/data-model/scrollback.md`). Argv, temp files, cwd, and
//! environment resolve client-side; [`SpawnArgs`] target the local daemon
//! even when attached to a remote host ([`crate::connector::Carrier`]).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use felis_protocol::messages::{GridDims, RegionPosition};
use felis_protocol::{SessionHex, messages::SpawnArgs};

/// The session a `pipe` / `run` chord fired on, exported to the spawned
/// command as [`ORIGIN_SESSION_ENV`], [`HOST_ENV`], [`CWD_ENV`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Origin {
    pub session_id: u128,
    /// The window's ssh destination; `None` on a local daemon.
    pub host: Option<String>,
    /// The verbatim `OSC 7` report: it describes a path on the origin's
    /// host, the one machine that can judge it.
    pub osc7: Option<String>,
}

/// Distinct from `FELIS_SESSION_ID`, which names the transient itself.
pub const ORIGIN_SESSION_ENV: &str = "FELIS_ORIGIN_SESSION_ID";
/// Unset for a local window, so a script branches on presence.
pub const HOST_ENV: &str = "FELIS_HOST";
/// The verbatim `OSC 7` report; the command runs here, the path may name
/// somewhere else.
pub const CWD_ENV: &str = "FELIS_CWD";
pub const TOP_LINE_ENV: &str = "FELIS_INPUT_LINE_NUMBER";
pub const CURSOR_LINE_ENV: &str = "FELIS_CURSOR_LINE";
pub const CURSOR_COLUMN_ENV: &str = "FELIS_CURSOR_COLUMN";

/// Every variable [`prepare_spawn`] may set. Each describes one chord, so
/// the daemon scrubs all of them from an inherited base and a stale value
/// never outlives the origin it named.
pub const ORIGIN_ENV_KEYS: [&str; 6] = [
    ORIGIN_SESSION_ENV,
    HOST_ENV,
    CWD_ENV,
    TOP_LINE_ENV,
    CURSOR_LINE_ENV,
    CURSOR_COLUMN_ENV,
];

/// A region file staged for a transient, unlinked on drop. A guard rather
/// than a path so the failure paths that end the handoff (a switch that
/// never lands, a socket that will not resolve) unlink it too.
#[derive(Debug)]
pub struct StagedRegion {
    path: PathBuf,
}

impl StagedRegion {
    #[must_use]
    pub const fn adopt(path: PathBuf) -> Self {
        Self { path }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StagedRegion {
    fn drop(&mut self) {
        remove_temp_file(&self.path);
    }
}

#[derive(Debug)]
#[non_exhaustive]
pub struct TransientSpawn {
    pub args: SpawnArgs,
    /// `None` for `run`, which is fed no region.
    pub region: Option<StagedRegion>,
}

/// The staged region's path is appended to the argv, the shape every
/// pager and picker expects. An empty `argv` selects the default pager.
pub fn prepare_spawn(
    region: Option<&[u8]>,
    argv: &[String],
    dims: GridDims,
    position: Option<RegionPosition>,
    origin: &Origin,
) -> std::io::Result<TransientSpawn> {
    let region = match region {
        Some(region) => Some(StagedRegion::adopt(write_region_file(region, None)?)),
        None => None,
    };

    let mut argv: Vec<String> = if argv.is_empty() {
        default_pager(position)
    } else {
        argv.to_vec()
    };
    let command = argv.remove(0);
    let mut args = argv;
    if let Some(staged) = &region {
        // Not `to_string_lossy`: `SpawnArgs.args` is a wire `string`, so
        // a `TMPDIR` that is not UTF-8 would hand the pager a path that
        // does not open rather than an error.
        let path = staged.path();
        let text = path.to_str().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "the staged region path `{}` is not valid UTF-8",
                    path.display()
                ),
            )
        })?;
        args.push(text.to_owned());
    }

    Ok(TransientSpawn {
        args: SpawnArgs {
            command,
            args,
            cwd: local_cwd_from_osc7(origin.osc7.as_deref().unwrap_or_default()),
            env: child_env(position, origin),
            dims: Some(dims.into()),
            tags: Vec::new(),
            // Whoever dials captures the env base, and staging happens
            // well before the dial.
            env_base: None,
        },
        region,
    })
}

fn child_env(position: Option<RegionPosition>, origin: &Origin) -> Vec<(String, String)> {
    let mut env = position_env(position);
    env.push((
        ORIGIN_SESSION_ENV.to_owned(),
        SessionHex(origin.session_id).to_string(),
    ));
    if let Some(host) = &origin.host {
        env.push((HOST_ENV.to_owned(), host.clone()));
    }
    if let Some(cwd) = &origin.osc7 {
        env.push((CWD_ENV.to_owned(), cwd.clone()));
    }
    env
}

/// Sets position env vars (`docs/reference/keybindings.md`). Unset rather than
/// a placeholder when unanchored. Uses environment variables rather than argv
/// token substitution to keep argv uninterpreted (principle 4).
fn position_env(position: Option<RegionPosition>) -> Vec<(String, String)> {
    position.map_or_else(Vec::new, |p| {
        vec![
            (TOP_LINE_ENV.to_owned(), p.top_line.to_string()),
            (CURSOR_LINE_ENV.to_owned(), p.cursor_line.to_string()),
            (CURSOR_COLUMN_ENV.to_owned(), p.cursor_column.to_string()),
        ]
    })
}

/// The `+N` start line rides the built-in default only: a user's `$PAGER`
/// is an opaque program whose argv felis must not invent flags for; it
/// reads the position off the environment like any configured command.
fn default_pager(position: Option<RegionPosition>) -> Vec<String> {
    match std::env::var("PAGER") {
        Ok(p) if !p.trim().is_empty() => {
            // Whitespace tokens, not shell parsing (principle 1): enough
            // for the near-universal `PAGER="less -R"` without exec'ing a
            // program literally named "less -R".
            p.split_whitespace().map(str::to_owned).collect()
        }
        _ => builtin_pager(position),
    }
}

fn builtin_pager(position: Option<RegionPosition>) -> Vec<String> {
    let mut argv = vec!["less".to_owned(), "-R".to_owned()];
    if let Some(position) = position {
        argv.push(format!("+{}", position.top_line));
    }
    argv
}

/// Empty when the report names no directory this machine can act on. The
/// host must be this machine: with a remote-attached window a foreign
/// path may exist here and mean something else (both machines have a
/// checkout at the same path). Only the `file://` form is read; a bare
/// path would be a guess about what the shell meant (principle 4).
#[must_use]
pub fn local_cwd_from_osc7(reported: &str) -> String {
    let Some((host, path)) = reported
        .strip_prefix("file://")
        .and_then(|r| r.split_once('/'))
    else {
        return String::new();
    };
    if !host_is_this_machine(host) {
        return String::new();
    }
    match percent_decode(&format!("/{path}")).map(|decoded| platform_path(&decoded)) {
        Some(path) if Path::new(&path).is_dir() => path,
        _ => String::new(),
    }
}

/// A Windows shell reports `file:///C:/src` for `C:\src`; Win32 needs the
/// drive letter leading (RFC 8089 Appendix E.2) and native separators.
#[cfg(windows)]
fn platform_path(decoded: &str) -> String {
    let is_drive = |rest: &str| {
        let mut chars = rest.chars();
        chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.next() == Some(':')
    };
    let rooted = match decoded.strip_prefix('/') {
        Some(rest) if is_drive(rest) => rest,
        _ => decoded,
    };
    rooted.replace('/', "\\")
}

#[cfg(unix)]
fn platform_path(decoded: &str) -> String {
    decoded.to_owned()
}

/// First label only: a shell reports `$HOSTNAME` while `uname` may carry
/// a domain suffix, or the reverse.
fn host_is_this_machine(host: &str) -> bool {
    if host.is_empty() || host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let label = |s: &str| s.split('.').next().unwrap_or_default().to_ascii_lowercase();
    nodename().is_some_and(|n| !n.is_empty() && label(&n) == label(host))
}

#[cfg(unix)]
fn nodename() -> Option<String> {
    rustix::system::uname()
        .nodename()
        .to_str()
        .ok()
        .map(str::to_owned)
}

#[cfg(windows)]
fn nodename() -> Option<String> {
    std::env::var("COMPUTERNAME").ok()
}

fn percent_decode(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `path` `None` means a fresh file under this client's temp dir.
pub fn write_region_file(region: &[u8], path: Option<PathBuf>) -> std::io::Result<PathBuf> {
    // The temp directory is pid-namespaced, so a counter suffices.
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let path = if let Some(path) = path {
        path
    } else {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir)?;
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        dir.join(format!("region-{unique:016x}.txt"))
    };
    std::fs::write(&path, region)?;
    Ok(path)
}

/// The pid in the name is how a later run's [`sweep_temp_dir`] tells a
/// directory whose owner is gone from one still in use.
const TEMP_DIR_PREFIX: &str = "felis-pipe-";

fn temp_dir() -> PathBuf {
    std::env::temp_dir().join(format!("{TEMP_DIR_PREFIX}{}", std::process::id()))
}

/// Startup reclaim of staging directories a crashed run left behind;
/// nothing else would ever remove them.
pub fn sweep_temp_dir() {
    let ours = std::process::id();
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| is_reclaimable(name, ours))
        {
            std::fs::remove_dir_all(entry.path()).ok();
        }
    }
}

/// A pid we cannot signal counts as live: on a shared `/tmp` that
/// directory belongs to another user's client, whose pager may be reading
/// the file.
fn is_reclaimable(name: &str, ours: u32) -> bool {
    let Some(pid) = name
        .strip_prefix(TEMP_DIR_PREFIX)
        .and_then(|pid| pid.parse::<u32>().ok())
    else {
        return false;
    };
    pid == ours || !process_is_live(pid)
}

#[cfg(unix)]
fn process_is_live(pid: u32) -> bool {
    let Some(pid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return false;
    };
    !matches!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH),
    )
}

/// Windows has no comparably cheap liveness test.
#[cfg(windows)]
#[expect(
    clippy::missing_const_for_fn,
    reason = "one signature with the Unix branch, which queries the OS"
)]
fn process_is_live(_pid: u32) -> bool {
    true
}

fn remove_temp_file(path: &Path) {
    std::fs::remove_file(path).ok();
    // Nothing else removes the staging directory at exit; `remove_dir`
    // refuses a non-empty one, so a sibling region still staged keeps it.
    if path.parent() == Some(temp_dir().as_path()) {
        std::fs::remove_dir(temp_dir()).ok();
    }
}

/// A background `run` (`docs/reference/keybindings.md`): no PTY, so the
/// client spawns the argv itself and only reaps it. `None` for an empty
/// argv, which the loader already drops.
#[must_use]
pub fn background_command(argv: &[String], origin: &Origin) -> Option<std::process::Command> {
    use std::process::Stdio;
    let (program, args) = argv.split_first()?;
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(dir) = background_cwd(origin) {
        command.current_dir(dir);
    }
    for key in ORIGIN_ENV_KEYS {
        command.env_remove(key);
    }
    command.envs(child_env(None, origin));
    detach(&mut command);
    Some(command)
}

/// Home rather than the client's own cwd, which is `/` for a window
/// macOS launched, the same fallback a window launch takes.
fn background_cwd(origin: &Origin) -> Option<PathBuf> {
    let local = local_cwd_from_osc7(origin.osc7.as_deref().unwrap_or_default());
    if local.is_empty() {
        directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_owned())
    } else {
        Some(PathBuf::from(local))
    }
}

#[cfg(unix)]
fn detach(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
}

#[cfg(windows)]
fn detach(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

/// Reaped on a thread of its own, so a long-lived command (a window
/// launch) neither leaves a zombie nor holds the event loop.
///
/// # Errors
/// The spawn's own error: a missing program, a cwd that vanished.
pub fn spawn_background(argv: &[String], origin: &Origin) -> std::io::Result<()> {
    let Some(mut command) = background_command(argv, origin) else {
        return Ok(());
    };
    let mut child = command.spawn()?;
    let program = argv[0].clone();
    std::thread::Builder::new()
        .name("felis-run-reaper".to_owned())
        .spawn(move || match child.wait() {
            Ok(status) if status.success() => {}
            Ok(status) => tracing::warn!(%program, %status, "background run exited unsuccessfully"),
            Err(err) => tracing::warn!(%program, %err, "background run could not be reaped"),
        })?;
    Ok(())
}

#[cfg(test)]
mod tests;

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
            ("FELIS_INPUT_LINE_NUMBER".to_owned(), p.top_line.to_string()),
            ("FELIS_CURSOR_LINE".to_owned(), p.cursor_line.to_string()),
            (
                "FELIS_CURSOR_COLUMN".to_owned(),
                p.cursor_column.to_string(),
            ),
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

#[cfg(test)]
mod tests;

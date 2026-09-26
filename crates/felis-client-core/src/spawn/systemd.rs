//! Hand-off of a cold-socket daemon to the systemd user manager
//! (`docs/explanation/architecture/overview.md` "Where an
//! auto-spawned daemon lands"). Best effort: everything here reports
//! "not done", and the caller forks as it always has.

use std::{
    ffi::{OsStr, OsString},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::Path,
    process::{Output, Stdio},
    time::Duration,
};

use felis_transport::retry::RetryPolicy;
use tokio::process::Command;

/// One unit per socket path, so a `--socket` daemon and the default
/// daemon coexist under the manager as they do when forked.
pub(super) const UNIT_PREFIX: &str = "felis-daemon-";

/// `--expand-environment=` exists from this release on; below it, a `$`
/// in an argument is expanded and has to be written doubled.
const EXPAND_ENVIRONMENT_SINCE: u32 = 254;

/// The start job's budget, mirrored by
/// [`RetryPolicy::MANAGED_BOOT`](felis_transport::retry::RetryPolicy::MANAGED_BOOT).
const START_TIMEOUT: &str = "15s";

/// Every helper here is a D-Bus round trip against the user manager, and
/// a bus that never answers must not hold a window open: each call is
/// bounded, and a breached bound falls back like a failure.
const HELPER_BUDGET: Duration = Duration::from_secs(3);

/// The start call blocks until the service reports ready, so its bound
/// clears `TimeoutStartSec` with room for the manager's own answer.
const START_BUDGET: Duration = Duration::from_secs(20);

pub(super) fn unit_name(socket: &Path) -> String {
    format!(
        "{UNIT_PREFIX}{:016x}",
        fnv1a64(socket.as_os_str().as_bytes())
    )
}

/// FNV-1a over the socket path: a name, not a digest. What it must give
/// is one stable unit per path, short enough to stay inside the unit-name
/// limit and free of the characters systemd escapes.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// True when this process runs under the systemd user manager, which is
/// the only placement #261 reports: the daemon inherits the launcher's
/// unit, and a compositor unit's `OOMPolicy=stop` ends the session when
/// a shell child is OOM-killed.
pub(super) fn under_user_manager(cgroup_file: &str, uid: u32) -> bool {
    let marker = format!("/user@{uid}.service/");
    cgroup_file
        .lines()
        .filter_map(|line| line.strip_prefix("0::"))
        .any(|path| path.contains(&marker))
}

/// `active` and `activating` both mean a concurrent launcher's start job
/// holds the unit name; every other state (a failed start, no such unit)
/// leaves the caller to fork.
pub(super) fn start_job_holds_the_name(active_state: &str) -> bool {
    matches!(active_state.trim(), "active" | "activating")
}

/// `systemd 261 (261.1-1)` → `261`.
pub(super) fn systemd_version(version_output: &str) -> Option<u32> {
    version_output
        .lines()
        .next()?
        .split_whitespace()
        .find_map(|token| token.parse().ok())
}

pub(super) fn supports_expand_environment(version: Option<u32>) -> bool {
    version.is_some_and(|version| version >= EXPAND_ENVIRONMENT_SINCE)
}

/// The transient unit: the launcher's own binary on the launcher's
/// socket, in `app.slice` with the `OOMPolicy=continue` that the service
/// default (`stop`) would otherwise reproduce one level down.
/// `RefuseManualStop=yes` leaves `felis daemon stop` the only way down,
/// and `--collect` unloads a failed start so the name stays reusable.
pub(super) fn systemd_run_argv(
    unit: &str,
    program: &OsStr,
    socket: &Path,
    expand_environment_flag: bool,
) -> Vec<OsString> {
    let mut argv: Vec<OsString> = vec![
        "--user".into(),
        format!("--unit={unit}").into(),
        "--service-type=notify".into(),
        "--collect".into(),
        "--quiet".into(),
    ];
    if expand_environment_flag {
        argv.push("--expand-environment=no".into());
    }
    for property in [
        "OOMPolicy=continue",
        "Slice=app.slice",
        "RefuseManualStop=yes",
    ] {
        argv.push("-p".into());
        argv.push(property.into());
    }
    argv.push("-p".into());
    argv.push(format!("TimeoutStartSec={START_TIMEOUT}").into());
    argv.push(program.to_os_string());
    argv.push("serve".into());
    argv.push("--socket".into());
    argv.push(if expand_environment_flag {
        socket.as_os_str().to_os_string()
    } else {
        double_dollars(socket.as_os_str())
    });
    argv
}

/// Without `--expand-environment=no` the manager expands `$` in an
/// argument, and `$$` is how a literal one is written.
fn double_dollars(value: &OsStr) -> OsString {
    let mut out = Vec::with_capacity(value.as_bytes().len());
    for byte in value.as_bytes() {
        if *byte == b'$' {
            out.push(b'$');
        }
        out.push(*byte);
    }
    OsString::from_vec(out)
}

/// What a `systemd-run` attempt leaves the launcher to do next.
#[derive(Debug)]
pub(super) enum Start {
    /// The manager started the daemon and it reported ready.
    Started,
    /// The name may be held by a concurrent launcher's start job, so the
    /// unit's state decides between waiting and forking. A breached
    /// budget lands here rather than in `Unavailable`: a manager too slow
    /// to answer is not a manager that is absent.
    Contested(String),
    /// No manager was reached at all, so nothing was asked of one.
    Unavailable(String),
}

/// A helper that could not be run at all, told apart from one that ran
/// and refused.
#[derive(Debug)]
enum HelperError {
    Unavailable(std::io::Error),
    TimedOut,
}

/// Why the launcher forked, at the two volumes the reasons deserve.
#[derive(Debug)]
pub(super) enum Fallback {
    /// The manager was never asked; the ordinary case off a desktop.
    NotAsked(String),
    /// The manager was asked and did not produce a reachable daemon.
    Failed(String),
    /// The endpoint answered with a failure that is no evidence it is
    /// free, so forking would spawn beside whatever holds it.
    Undialable(crate::connector::ConnectError),
}

/// The launcher's view of the manager, injected whole so tests can drive
/// every branch without a live user manager.
pub(super) struct HandOff {
    pub(super) systemd_run: OsString,
    pub(super) systemctl: OsString,
    pub(super) cgroup: String,
    pub(super) uid: u32,
    pub(super) program: OsString,
    pub(super) helper_budget: Duration,
    pub(super) start_budget: Duration,
    pub(super) boot: RetryPolicy,
    pub(super) managed_boot: RetryPolicy,
}

impl HandOff {
    pub(super) fn production(program: OsString) -> Option<Self> {
        Some(Self {
            systemd_run: OsString::from("systemd-run"),
            systemctl: OsString::from("systemctl"),
            cgroup: std::fs::read_to_string("/proc/self/cgroup").ok()?,
            uid: rustix::process::geteuid().as_raw(),
            program,
            helper_budget: HELPER_BUDGET,
            start_budget: START_BUDGET,
            boot: RetryPolicy::DAEMON_BOOT,
            managed_boot: RetryPolicy::MANAGED_BOOT,
        })
    }

    pub(super) fn under_manager(&self) -> bool {
        under_user_manager(&self.cgroup, self.uid)
    }

    /// Asks the manager to start the daemon, and blocks until the start
    /// job completes: with `Type=notify` its success means the daemon
    /// bound the socket.
    pub(super) async fn start(&self, unit: &str, socket: &Path) -> Start {
        let expand = supports_expand_environment(self.version().await);
        let argv = systemd_run_argv(unit, &self.program, socket, expand);
        let mut command = captured(&self.systemd_run);
        command.args(&argv);
        classify_start(
            run(&mut command, self.start_budget).await,
            self.start_budget,
        )
    }

    /// The one state query the launcher makes before it forks.
    pub(super) async fn active_state(&self, unit: &str) -> Option<String> {
        let mut command = captured(&self.systemctl);
        command
            .args(["--user", "show", "-p", "ActiveState", "--value"])
            .arg(unit);
        let out = run(&mut command, self.helper_budget).await.ok()?;
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    async fn version(&self) -> Option<u32> {
        let mut command = captured(&self.systemd_run);
        command.arg("--version");
        let out = run(&mut command, self.helper_budget).await.ok()?;
        systemd_version(&String::from_utf8_lossy(&out.stdout))
    }
}

/// Both streams are captured, never inherited: `systemd-run`'s progress
/// and diagnostics must not reach the stdout of a point verb whose
/// `--format json` promises its result object and nothing else.
fn captured(program: &OsStr) -> Command {
    let mut command = Command::new(program);
    command.stdin(Stdio::null());
    command
}

/// A timed-out helper is killed rather than left to finish into a pipe
/// nobody reads.
async fn run(command: &mut Command, budget: Duration) -> Result<Output, HelperError> {
    command.kill_on_drop(true);
    match tokio::time::timeout(budget, command.output()).await {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(err)) => Err(HelperError::Unavailable(err)),
        Err(_) => Err(HelperError::TimedOut),
    }
}

fn classify_start(result: Result<Output, HelperError>, budget: Duration) -> Start {
    match result {
        Ok(out) if out.status.success() => Start::Started,
        Ok(out) => Start::Contested(format!(
            "systemd-run exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(HelperError::TimedOut) => {
            Start::Contested(format!("systemd-run did not answer within {budget:?}"))
        }
        Err(HelperError::Unavailable(err)) => {
            Start::Unavailable(format!("systemd-run could not be run ({err})"))
        }
    }
}

/// Stub helpers for the branch tests: a script on disk, addressed by
/// path, so no test has to mutate the process environment.
#[cfg(test)]
pub(super) fn stub(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh
{body}
"
        ),
    )
    .expect("write stub");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod stub");
    path
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::path::PathBuf;

    const NIRI: &str = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/niri.service\n";

    #[test]
    fn a_launcher_inside_the_user_manager_is_recognized() {
        assert!(under_user_manager(NIRI, 1000));
    }

    #[test]
    fn a_login_session_scope_is_not_the_user_manager() {
        let scope = "0::/user.slice/user-1000.slice/session-3.scope\n";
        assert!(!under_user_manager(scope, 1000));
    }

    #[test]
    fn another_users_manager_does_not_count_as_this_ones() {
        assert!(!under_user_manager(NIRI, 1001));
    }

    #[test]
    fn a_cgroup_namespace_that_hides_the_manager_is_not_recognized() {
        assert!(!under_user_manager("0::/\n", 1000));
    }

    #[test]
    fn a_legacy_v1_hierarchy_is_not_read_as_the_unified_line() {
        let v1 = "1:name=systemd:/user.slice/user-1000.slice/user@1000.service/app.slice\n";
        assert!(!under_user_manager(v1, 1000));
    }

    #[test]
    fn two_socket_paths_get_two_unit_names() {
        let one = unit_name(Path::new("/run/user/1000/felis/daemon.sock"));
        let other = unit_name(Path::new("/run/user/1000/felis/other.sock"));
        assert_ne!(one, other);
        assert_eq!(
            one,
            unit_name(Path::new("/run/user/1000/felis/daemon.sock"))
        );
        assert!(
            one.strip_prefix(UNIT_PREFIX)
                .is_some_and(|hash| hash.len() == 16
                    && hash
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())),
            "got {one}",
        );
    }

    #[test]
    fn the_hash_matches_the_fnv1a_reference_vector() {
        // FNV-1a 64 of "a" (the published test vector), so a rename of
        // the function cannot quietly change every unit name.
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn the_argv_carries_the_socket_the_launcher_resolved_and_every_property() {
        let socket = PathBuf::from("/run/user/1000/felis/daemon.sock");
        let unit = unit_name(&socket);
        let argv = systemd_run_argv(
            &unit,
            OsStr::new("/nix/store/x/bin/felis-daemon"),
            &socket,
            true,
        );
        let shown: Vec<String> = argv
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            shown,
            vec![
                "--user".to_owned(),
                format!("--unit={unit}"),
                "--service-type=notify".to_owned(),
                "--collect".to_owned(),
                "--quiet".to_owned(),
                "--expand-environment=no".to_owned(),
                "-p".to_owned(),
                "OOMPolicy=continue".to_owned(),
                "-p".to_owned(),
                "Slice=app.slice".to_owned(),
                "-p".to_owned(),
                "RefuseManualStop=yes".to_owned(),
                "-p".to_owned(),
                "TimeoutStartSec=15s".to_owned(),
                "/nix/store/x/bin/felis-daemon".to_owned(),
                "serve".to_owned(),
                "--socket".to_owned(),
                "/run/user/1000/felis/daemon.sock".to_owned(),
            ],
        );
    }

    #[test]
    fn a_dollar_in_the_socket_is_doubled_only_where_the_manager_expands_it() {
        let socket = PathBuf::from("/tmp/felis$test/daemon.sock");
        let expanded = systemd_run_argv("unit", OsStr::new("felis-daemon"), &socket, false);
        assert_eq!(
            expanded.last().unwrap().to_string_lossy(),
            "/tmp/felis$$test/daemon.sock"
        );
        let literal = systemd_run_argv("unit", OsStr::new("felis-daemon"), &socket, true);
        assert_eq!(
            literal.last().unwrap().to_string_lossy(),
            "/tmp/felis$test/daemon.sock"
        );
        assert!(!expanded.iter().any(|arg| arg == "--expand-environment=no"));
    }

    #[test]
    fn the_expand_environment_flag_follows_the_reported_version() {
        assert_eq!(
            systemd_version("systemd 261 (261.1-1)\n+PAM +AUDIT\n"),
            Some(261)
        );
        assert_eq!(systemd_version("systemd 253 (253.6)\n"), Some(253));
        assert_eq!(systemd_version(""), None);
        assert!(supports_expand_environment(Some(254)));
        assert!(!supports_expand_environment(Some(253)));
        assert!(!supports_expand_environment(None));
    }

    fn output(code: i32, stderr: &str) -> Output {
        use std::os::unix::process::ExitStatusExt;
        Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn a_clean_start_is_the_only_outcome_that_needs_nothing_more() {
        assert!(matches!(
            classify_start(Ok(output(0, "")), HELPER_BUDGET),
            Start::Started
        ));
    }

    #[test]
    fn a_refused_start_carries_the_managers_own_words_into_the_reason() {
        let Start::Contested(reason) =
            classify_start(Ok(output(1, "Unit is masked.")), HELPER_BUDGET)
        else {
            panic!("a non-zero exit leaves the unit's state to decide");
        };
        assert!(reason.contains("Unit is masked."), "got {reason}");
    }

    /// A manager too slow to answer is not a manager that is absent: the
    /// launcher still checks the unit before it forks, and the line it
    /// writes is a warning rather than a debug note.
    #[test]
    fn a_breached_budget_is_classified_as_a_failed_attempt_not_an_absent_manager() {
        let Start::Contested(reason) = classify_start(Err(HelperError::TimedOut), HELPER_BUDGET)
        else {
            panic!("a timeout must not read as an absent manager");
        };
        assert!(reason.contains("did not answer"), "got {reason}");
    }

    #[test]
    fn a_missing_systemd_run_is_no_attempt_at_all() {
        let err = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(matches!(
            classify_start(Err(HelperError::Unavailable(err)), HELPER_BUDGET),
            Start::Unavailable(_)
        ));
    }

    #[tokio::test]
    async fn a_helper_that_never_exits_is_bounded_and_killed() {
        let tmp = tempfile::TempDir::new().unwrap();
        // `exec` so the kill lands on the sleep itself: a shell that
        // merely waits on it would leave the child behind.
        let hang = stub(tmp.path(), "hang", "exec sleep 60");
        let mut command = Command::new(&hang);
        command.stdin(Stdio::null());
        let started = std::time::Instant::now();
        let result = run(&mut command, Duration::from_millis(150)).await;
        assert!(matches!(result, Err(HelperError::TimedOut)));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the budget must bound the wait, not the helper"
        );
    }

    #[test]
    fn only_a_running_start_job_is_worth_waiting_for() {
        assert!(start_job_holds_the_name("active\n"));
        assert!(start_job_holds_the_name("activating"));
        assert!(!start_job_holds_the_name("failed\n"));
        assert!(!start_job_holds_the_name("inactive\n"));
        assert!(!start_job_holds_the_name(""));
    }
}

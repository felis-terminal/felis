//! `felis doctor`: checklist across every layer of the installation.
//!
//! Exits 0 or 1, treating an unreachable daemon as a finding rather than failure.
//! GUI probes run via `felis-client --doctor-probe` to keep this front door GPU-free.

#![expect(
    clippy::print_stdout,
    reason = "this module renders the human framing of a report"
)]

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

use felis_client_core::config::GUI_CLIENT_ID;
use felis_client_core::doctor::{
    DOCTOR_PROBE_FLAG, DOCTOR_REPORT_PROBE_FLAG, PROBE_VERSION, ProbeReport,
};
use felis_client_core::local_socket::SocketSource;
use felis_client_core::{
    BoundedDialError, ConfigSource, ConnectError, EffectiveConfig, Reconnector, RemoteSpawn,
    dial_bounded,
};

use felis_protocol::BuildIdentity;
use felis_transport::preface::{PROBE_DEADLINE, ProbeOutcome, connect_error_is_absent, probe};

use crate::cli_output::{CheckObject, DoctorReportResult, DoctorResult, PointFormat, Reporter};
use crate::cli_report::{self, HomeCollapse};
use crate::conn::Resolved;

/// The terminfo entry the daemon stamps into every session. Spelled
/// here rather than imported because felis-cli must not depend on
/// felis-daemon; the name is frozen user-facing identity
/// (`docs/reference/terminal-identity.md`).
const FELIS_TERM: &str = "xterm-felis";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    /// Degraded but felis works.
    Warn,
    Fail,
    /// The check does not apply here. Not `ok`: "not checked" must not
    /// read as "checked and fine".
    Skipped,
}

impl Status {
    const fn token(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
            Self::Skipped => "skipped",
        }
    }
}

struct Check {
    name: &'static str,
    status: Status,
    detail: String,
}

impl Check {
    fn new(name: &'static str, status: Status, detail: impl Into<String>) -> Self {
        Self {
            name,
            status,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, clap::Subcommand)]
pub(crate) enum DoctorOp {
    /// Print the checklist and this machine's environment as Markdown
    /// to paste into a bug report.
    ///
    /// Run it from the affected window and review it before posting;
    /// exits 0 whenever the report is written, failing checks included.
    Report {
        #[command(flatten)]
        output: PointFormat,
    },
}

impl DoctorOp {
    pub(crate) const fn format(&self) -> crate::cli_output::Format {
        match self {
            Self::Report { output } => output.format,
        }
    }
}

/// Grep-shaped like `config check`: `1` means "something here is
/// broken", and the report is on stdout either way.
pub(crate) fn run(
    runtime: &tokio::runtime::Runtime,
    output: &PointFormat,
    target: &Resolved,
    config_source: &ConfigSource,
    client_program: impl FnOnce() -> OsString,
) -> i32 {
    let out = Reporter::point(output.format);
    let gathered = gather(runtime, target, config_source, || {
        run_probe(&client_program(), DOCTOR_PROBE_FLAG.as_ref())
    });
    let checks = gathered.checks;
    let failed = count(&checks, Status::Fail);
    let warned = count(&checks, Status::Warn);

    if out.machine() {
        out.result(&DoctorResult {
            failed,
            warned,
            checks: check_objects(&checks),
        });
    } else {
        print_checks(&checks);
    }
    i32::from(failed > 0)
}

/// Exits `0` whenever the report is written: a failing check is what a
/// reporter came to report, not a failure of the verb.
pub(crate) fn run_report(
    runtime: &tokio::runtime::Runtime,
    output: &PointFormat,
    target: &Resolved,
    config_source: &ConfigSource,
    client_program: impl FnOnce() -> OsString,
) -> i32 {
    let out = Reporter::point(output.format);
    let mut fonts_note = None;
    let gathered = gather(runtime, target, config_source, || {
        let (probe, note) = run_report_probe(&client_program(), config_source);
        fonts_note = note;
        probe
    });
    let home = HomeCollapse::discover();
    let daemon_detail = gathered
        .checks
        .first()
        .map_or_else(String::new, |row| home.apply(&row.detail));
    let environment = cli_report::environment(
        &cli_report::Inputs {
            probe: &gathered.probe,
            fonts_note: fonts_note.as_deref(),
            daemon: gathered.daemon.as_ref(),
            config_source,
        },
        &cli_report::process_env,
    );
    let result = home.redact(DoctorReportResult {
        failed: count(&gathered.checks, Status::Fail),
        warned: count(&gathered.checks, Status::Warn),
        checks: check_objects(&gathered.checks),
        environment,
    });
    let result = match result {
        Ok(result) => result,
        Err(err) => return out.fail(crate::cli_output::ErrorKind::Internal, err),
    };
    if out.machine() {
        out.result(&result);
    } else {
        print!("{}", cli_report::markdown(&result, &daemon_detail));
    }
    0
}

fn count(checks: &[Check], status: Status) -> u64 {
    checks.iter().filter(|c| c.status == status).count() as u64
}

fn check_objects(checks: &[Check]) -> Vec<CheckObject> {
    checks
        .iter()
        .map(|c| CheckObject {
            check: c.name.to_owned(),
            status: c.status.token().to_owned(),
            detail: c.detail.clone(),
        })
        .collect()
}

struct Gathered {
    checks: Vec<Check>,
    daemon: Option<BuildIdentity>,
    probe: Result<ProbeReport, String>,
}

fn gather(
    runtime: &tokio::runtime::Runtime,
    target: &Resolved,
    config_source: &ConfigSource,
    probe: impl FnOnce() -> Result<ProbeReport, String>,
) -> Gathered {
    let local = local_endpoint(target);
    let bounded = local
        .as_ref()
        .filter(|(source, _)| *source == SocketSource::Default)
        .map(|(_, path)| path.as_path());
    let mut running = None;
    let (mut daemon, primary) =
        runtime.block_on(daemon_check_reading(&target.target, bounded, &mut running));
    // The installed binary is this machine's, so only a local daemon is
    // compared with it.
    if local.is_some()
        && let Some(running) = &running
        && let Some(note) = installed_skew(running, installed_daemon_identity().as_ref())
    {
        daemon.detail = format!("{}; {note}", daemon.detail);
    }
    let endpoints = local
        .as_ref()
        .map_or_else(EndpointReport::default, |(source, path)| {
            runtime.block_on(endpoint_report(*source, path, &primary))
        });
    if let Some(note) = endpoints.note {
        daemon.detail = format!("{}; {note}", daemon.detail);
    }
    let mut checks = vec![daemon];
    checks.extend(endpoints.rows);
    checks.push(config_check(config_source));
    checks.push(terminfo_check());
    let probe = probe();
    let (gpu, clipboard) = probe_rows(&probe);
    checks.push(gpu);
    checks.push(clipboard);
    checks.push(remote_helper_check());
    Gathered {
        checks,
        daemon: running,
        probe,
    }
}

fn installed_daemon_identity() -> Option<BuildIdentity> {
    let out = Command::new(felis_client_core::installed_daemon()?)
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    crate::cli_version::identity_from_version_line(&String::from_utf8_lossy(&out.stdout))
}

/// A running daemon built other than the installed `felis-daemon` stays
/// so until an upgrade replaces it
/// (`docs/explanation/architecture/overview.md` "In-place upgrade").
fn installed_skew(running: &BuildIdentity, installed: Option<&BuildIdentity>) -> Option<String> {
    let installed = installed.filter(|installed| *installed != running)?;
    Some(format!(
        "the installed felis-daemon is {}: `felis daemon upgrade` switches to it and keeps every \
         session",
        installed.human()
    ))
}

fn print_checks(checks: &[Check]) {
    let width = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
    for check in checks {
        println!(
            "{:<7} {:<width$}  {}",
            check.status.token(),
            check.name,
            check.detail
        );
    }
}

/// `doctor` reports on the daemon, so it must never be the reason one
/// exists (docs/reference/cli.md "Auto-spawning").
pub(crate) const REMOTE_SPAWN: RemoteSpawn = RemoteSpawn::Refuse;

/// The dialed endpoint and where its address came from, on the local
/// Unix carrier only (`docs/reference/cli.md` "Doctor"): a deadline on
/// the SSH carrier would fire under ssh's own prompts, and a Windows
/// pipe name has no second endpoint to reason about.
fn local_endpoint(target: &Resolved) -> Option<(SocketSource, PathBuf)> {
    if !cfg!(unix) {
        return None;
    }
    let source = target.local_source?;
    Some((source, target.target.carrier.local_socket()?))
}

/// What the primary row established about the endpoint it dialed. Only
/// [`Self::Absent`] licenses a sibling row saying no daemon is there.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Primary {
    Live,
    Absent,
    /// The dial failed for a reason that says nothing about whether a
    /// daemon is there, or did not complete at all.
    Undialable {
        why: String,
    },
    /// Something accepted the connection and did not answer as a felis
    /// daemon.
    Unverified {
        why: String,
    },
}

impl Primary {
    /// What the `daemon` row established, for a sibling row that has to
    /// say why the target answered nothing.
    fn why(&self) -> &str {
        match self {
            Self::Live => "a felis daemon answered",
            Self::Absent => "nothing is listening there",
            Self::Undialable { why } | Self::Unverified { why } => why,
        }
    }
}

/// The read-side dial: `doctor` must never start the daemon it is
/// reporting on. The outcome travels beside the row, because the
/// sibling row's wording turns on what this dial could establish.
#[cfg(all(test, unix))]
async fn daemon_check(target: &Reconnector, bounded: Option<&Path>) -> (Check, Primary) {
    daemon_check_reading(target, bounded, &mut None).await
}

/// [`daemon_check`], also handing out the build a daemon that answered
/// reported.
async fn daemon_check_reading(
    target: &Reconnector,
    bounded: Option<&Path>,
    running: &mut Option<BuildIdentity>,
) -> (Check, Primary) {
    let dialed = match bounded {
        Some(resolved) => match dial_bounded(
            target.carrier.clone(),
            target.offer,
            REMOTE_SPAWN,
            PROBE_DEADLINE,
        )
        .await
        {
            Ok(conn) => Ok(conn),
            Err(BoundedDialError::Connect(err)) => Err(err),
            Err(expired @ BoundedDialError::ConnectTimedOut { .. }) => {
                return (
                    Check::new(
                        "daemon",
                        Status::Warn,
                        format!("could not dial {}: {expired}", resolved.display()),
                    ),
                    Primary::Undialable {
                        why: expired.to_string(),
                    },
                );
            }
            // The connect completed, so something is listening there
            // whatever the handshake never said.
            Err(expired @ BoundedDialError::HandshakeTimedOut { .. }) => {
                let why = expired.to_string();
                return (unverified(resolved, &why), Primary::Unverified { why });
            }
        },
        None => crate::conn::dial(target, REMOTE_SPAWN).await,
    };
    match dialed {
        Ok(conn) => {
            running.clone_from(&conn.daemon_identity);
            (
                Check::new(
                    "daemon",
                    Status::Ok,
                    // The *negotiated* minor, said so: labeling min(this build,
                    // the daemon's) as "wire" would let a reader diagnosing a
                    // skew read this build's own ceiling as the daemon's.
                    format!(
                        "running, build {}, negotiated wire {}.{} (`felis daemon status` reports the \
                     daemon's own)",
                        conn.daemon_identity
                            .as_ref()
                            .map_or_else(|| "unreported".to_owned(), BuildIdentity::human),
                        felis_protocol::preface::PROTOCOL_MAJOR,
                        conn.effective_minor,
                    ),
                ),
                Primary::Live,
            )
        }
        // A major-skewed daemon *did* answer; folding it into "not
        // running" would hide the one condition no retry can fix.
        Err(ConnectError::MajorMismatch {
            client_major,
            daemon_min,
            daemon_max,
        }) => (
            Check::new(
                "daemon",
                Status::Fail,
                format!(
                    "running, but speaks protocol major {daemon_min}-{daemon_max} \
                     and this build speaks {client_major}: restart the daemon on a matching build"
                ),
            ),
            Primary::Live,
        ),
        // Also a running daemon, and one no retry fixes: it answered the
        // offer with a major this build never sent.
        Err(ConnectError::AcceptedUnofferedMajor { offered, accepted }) => (
            Check::new(
                "daemon",
                Status::Fail,
                format!(
                    "running, but accepted protocol major {accepted} when this build offered \
                     {offered}: the daemon is not speaking felis's negotiation, rebuild both halves"
                ),
            ),
            Primary::Live,
        ),
        // A valid preface carrying a status this build cannot name is a
        // refusal from a live, newer daemon (docs/reference/ipc.md
        // "Version preface"), not silence.
        Err(ConnectError::UnknownPrefaceStatus { status, words, .. }) => (
            Check::new(
                "daemon",
                Status::Warn,
                format!(
                    "running, but answered preface status {status} (words {}, {}) this build does \
                     not know: upgrade this build",
                    words[0], words[1]
                ),
            ),
            Primary::Live,
        ),
        // Every refusal reason comes from a daemon that answered, so
        // none of them may read as "not running".
        Err(ConnectError::Refused { reason, detail }) => (
            Check::new(
                "daemon",
                Status::Warn,
                format!("running, but refused this connection ({reason:?}): {detail}"),
            ),
            Primary::Live,
        ),
        Err(err) => partition_dial_failure(&err, bounded),
    }
}

/// Classified on every target, because only `ENOENT` and `ECONNREFUSED`
/// prove nothing is listening; rendered that way on the default local
/// endpoint alone, every other target keeping the single "not running"
/// line its unbounded dial can honestly say
/// (`docs/reference/cli.md` "Doctor").
fn partition_dial_failure(err: &ConnectError, bounded: Option<&Path>) -> (Check, Primary) {
    let outcome = match err {
        ConnectError::Connect(io_err) if connect_error_is_absent(io_err) => Primary::Absent,
        ConnectError::Connect(_) => Primary::Undialable {
            why: err.to_string(),
        },
        _ => Primary::Unverified {
            why: err.to_string(),
        },
    };
    let Some(resolved) = bounded else {
        return (not_running(err, None), outcome);
    };
    let check = match &outcome {
        Primary::Absent => not_running(err, Some(resolved)),
        Primary::Undialable { .. } => Check::new(
            "daemon",
            Status::Warn,
            format!("could not dial {}: {err}", resolved.display()),
        ),
        Primary::Unverified { .. } | Primary::Live => unverified(resolved, &err.to_string()),
    };
    (check, outcome)
}

/// No daemon is the normal state before the first window. A refused
/// connect at a socket inode is what a stopped daemon leaves behind
/// (REQ-009d: exit unlinks nothing), so the row says the next start
/// replaces it rather than inviting an `rm`; anything else at the path
/// the next start would refuse, so it is named instead.
fn not_running(err: &ConnectError, endpoint: Option<&Path>) -> Check {
    let detail =
        refused_endpoint_detail(err, endpoint).unwrap_or_else(|| format!("not running ({err})"));
    Check::new(
        "daemon",
        Status::Warn,
        format!("{detail}; a window launch starts one"),
    )
}

#[cfg(unix)]
fn refused_endpoint_detail(err: &ConnectError, endpoint: Option<&Path>) -> Option<String> {
    use std::os::unix::fs::FileTypeExt as _;

    if !matches!(err, ConnectError::Connect(io_err)
        if io_err.kind() == std::io::ErrorKind::ConnectionRefused)
    {
        return None;
    }
    // A non-following `lstat`: a symlink at the endpoint is the user's,
    // and a start refuses it rather than replacing it.
    let path = endpoint?;
    let file_type = std::fs::symlink_metadata(path).ok()?.file_type();
    if file_type.is_socket() {
        return Some("not running (stale socket, replaced on the next start)".to_owned());
    }
    let what = if file_type.is_symlink() {
        "a symlink"
    } else if file_type.is_dir() {
        "a directory"
    } else if file_type.is_file() {
        "a regular file"
    } else {
        return None;
    };
    Some(format!("not running ({err}; {} is {what})", path.display()))
}

/// A pipe name is no path, so there is no inode to classify.
#[cfg(not(unix))]
const fn refused_endpoint_detail(_err: &ConnectError, _endpoint: Option<&Path>) -> Option<String> {
    None
}

fn unverified(resolved: &Path, why: &str) -> Check {
    Check::new(
        "daemon",
        Status::Warn,
        format!(
            "something listens at {} but did not answer as a felis daemon ({why})",
            resolved.display()
        ),
    )
}

/// What the endpoint section adds: a note on the `daemon` row, and one
/// `daemon-sibling` row per endpoint a felis daemon answered
/// (`docs/reference/cli.md` "Doctor").
#[derive(Default)]
struct EndpointReport {
    note: Option<String>,
    rows: Vec<Check>,
}

impl EndpointReport {
    const fn note(note: String) -> Self {
        Self {
            note: Some(note),
            rows: Vec::new(),
        }
    }
}

async fn endpoint_report(source: SocketSource, target: &Path, primary: &Primary) -> EndpointReport {
    if source == SocketSource::Explicit {
        return EndpointReport::default();
    }
    let default = felis_transport::socket::default_socket_path();
    endpoint_report_from(source, target, default, primary).await
}

/// A daemon the default endpoint answers while this shell is stamped
/// elsewhere is worth one row, because nothing on the wire identifies a
/// daemon instance: the row states what answered and never claims two
/// daemons.
async fn endpoint_report_from(
    source: SocketSource,
    target: &Path,
    default: std::io::Result<PathBuf>,
    primary: &Primary,
) -> EndpointReport {
    // An explicit `--socket` names one address on purpose; neither the
    // default nor the endpoints it is not answer a question it asked.
    if source == SocketSource::Explicit {
        return EndpointReport::default();
    }
    let default = match default {
        Ok(default) => default,
        // Said on every other provenance: a default that cannot be
        // resolved is the next command's failure, whatever this one
        // targeted.
        Err(err) => {
            return EndpointReport::note(format!("the default endpoint cannot be resolved: {err}"));
        }
    };
    if same_endpoint(target, &default) || source != SocketSource::Stamped {
        return EndpointReport::default();
    }
    let note = format!(
        "this shell targets {target}, which is not the endpoint processes without `FELIS_SOCKET` \
         resolve ({default}); commands typed in this shell keep targeting {target} unless run with \
         `env -u FELIS_SOCKET` or `--socket {default}`",
        target = target.display(),
        default = default.display(),
    );
    let mut report = EndpointReport::note(note);
    report.rows.push(sibling_row(
        target,
        &default,
        primary,
        probe(default.as_path(), PROBE_DEADLINE).await,
    ));
    report
}

/// The row reports the default's state either way: a reader who is
/// stamped elsewhere is asking whether the endpoint they are not on
/// holds a daemon, and "no row" answers that with silence.
fn sibling_row(target: &Path, default: &Path, primary: &Primary, found: ProbeOutcome) -> Check {
    let state = match found {
        ProbeOutcome::Live { detail } => return stranded_row(target, default, primary, detail),
        ProbeOutcome::Absent => "is cold".to_owned(),
        ProbeOutcome::Indeterminate { err } => format!("could not be dialed ({err})"),
        ProbeOutcome::ConnectedUnverified { why } => {
            format!("has something listening that did not answer as a felis daemon ({why})")
        }
    };
    Check::new(
        "daemon-sibling",
        Status::Ok,
        format!(
            "this shell targets {target}; the default endpoint {default}, which processes without \
             `FELIS_SOCKET` resolve, {state}",
            target = target.display(),
            default = default.display(),
        ),
    )
}

/// The stamp names an endpoint outside what the environment resolves,
/// so the row says which of the two answered.
fn stranded_row(target: &Path, default: &Path, primary: &Primary, detail: Option<String>) -> Check {
    let said = said(detail);
    let text = match primary {
        Primary::Live => format!(
            "a felis daemon also answers at {default}; if it is a different daemon, its sessions \
             are not visible from {target}. Inspect with `felis --socket {default} sessions \
             list`{said}",
            default = default.display(),
            target = target.display(),
        ),
        Primary::Absent | Primary::Undialable { .. } | Primary::Unverified { .. } => format!(
            "this shell targets {target}, where no daemon answered ({why}); a felis daemon answers \
             at {default}, the endpoint processes without `FELIS_SOCKET` resolve{said}",
            target = target.display(),
            default = default.display(),
            why = primary.why(),
        ),
    };
    Check::new("daemon-sibling", Status::Warn, text)
}

fn said(detail: Option<String>) -> String {
    detail.map_or_else(String::new, |detail| format!(" ({detail})"))
}

/// A symlinked runtime directory gives one socket two names.
fn same_endpoint(one: &Path, other: &Path) -> bool {
    canonical(one) == canonical(other)
}

/// Falls back to the parent so two names for one socket still compare
/// equal before either has been bound.
fn canonical(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    let named = path
        .file_name()
        .zip(path.parent().and_then(|parent| parent.canonicalize().ok()));
    named.map_or_else(|| path.to_path_buf(), |(name, dir)| dir.join(name))
}

/// Counts only: the full set is `felis config check`'s job, and
/// duplicating it would make `doctor` a worse `config check` instead
/// of a pointer to it.
fn config_check(source: &ConfigSource) -> Check {
    let Some(path) = source.path() else {
        return Check::new(
            "config",
            Status::Warn,
            "no config directory (felis found no home directory); built-in defaults are in use",
        );
    };
    let (_config, diagnostics) = EffectiveConfig::diagnose_source(source, GUI_CLIENT_ID);
    let errors = diagnostics.errors().count();
    let warnings = diagnostics.warnings().count();
    // A selected file that is absent is an error diagnostic instead.
    if !path.is_file() && errors == 0 {
        return Check::new(
            "config",
            Status::Ok,
            format!(
                "no file at {}; built-in defaults are in use",
                path.display()
            ),
        );
    }
    let where_ = path.display();
    match (errors, warnings) {
        (0, 0) => Check::new("config", Status::Ok, format!("{where_} is clean")),
        (0, w) => Check::new(
            "config",
            Status::Warn,
            format!("{where_}: {w} warning(s); run `felis config check` for the list"),
        ),
        (e, w) => Check::new(
            "config",
            Status::Fail,
            format!(
                "{where_}: {e} error(s), {w} warning(s); \
                 felis is using its built-in defaults. Run `felis config check`"
            ),
        ),
    }
}

/// A missing entry is invisible from inside felis: the daemon stamps
/// `TERM=xterm-felis` whether or not the database has it
/// (docs/how-to/fix-terminfo-problems.md).
#[cfg(unix)]
fn terminfo_check() -> Check {
    match terminfo_entry(FELIS_TERM) {
        Some(path) => Check::new(
            "terminfo",
            Status::Ok,
            format!("{FELIS_TERM} found at {}", path.display()),
        ),
        None => Check::new(
            "terminfo",
            Status::Fail,
            format!(
                "no {FELIS_TERM} entry in this machine's terminfo database; \
                 TUIs will degrade. See docs/how-to/fix-terminfo-problems.md \
                 (from a source checkout: `tic -x share/terminfo/felis.terminfo`)"
            ),
        ),
    }
}

/// Off Unix ncurses is not the consumer of `TERM`, so a miss would be
/// a false alarm.
#[cfg(not(unix))]
fn terminfo_check() -> Check {
    Check::new(
        "terminfo",
        Status::Skipped,
        format!("terminfo is a Unix ncurses database; nothing here reads {FELIS_TERM} from one"),
    )
}

/// Find a compiled terminfo entry following ncurses search order.
///
/// Reimplemented to avoid adding a C ncurses dependency to this front door.
#[cfg(unix)]
fn terminfo_entry(name: &str) -> Option<PathBuf> {
    let first = name.chars().next()?;
    // Two layouts in the wild: a letter directory, and the hex-of-byte
    // directory `tic` uses on filesystems that fold case.
    let leaves = [
        format!("{first}/{name}"),
        format!("{:x}/{name}", first as u32),
    ];

    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("TERMINFO") {
        roots.push(PathBuf::from(dir));
    }
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join(".terminfo"));
    }
    if let Some(dirs) = std::env::var_os("TERMINFO_DIRS") {
        for entry in std::env::split_paths(&dirs) {
            // An empty element means the compiled-in default.
            if !entry.as_os_str().is_empty() {
                roots.push(entry);
            }
        }
    }
    roots.extend(
        ["/usr/share/terminfo", "/lib/terminfo", "/etc/terminfo"]
            .into_iter()
            .map(PathBuf::from),
    );

    roots
        .into_iter()
        .flat_map(|root| leaves.iter().map(move |leaf| root.join(leaf)))
        .find(|candidate| candidate.is_file())
}

/// A missing frontend is [`Status::Skipped`], not a failure: a headless
/// install has no GUI binary by design, and telling that user their GPU
/// is broken would send them after a problem they do not have.
fn probe_rows(probe: &Result<ProbeReport, String>) -> (Check, Check) {
    let report = match probe {
        Ok(report) => report,
        Err(reason) => {
            return (
                Check::new("gpu", Status::Skipped, reason.clone()),
                Check::new("clipboard", Status::Skipped, reason.clone()),
            );
        }
    };

    let gpu = if report.gpu.available {
        // A CPU adapter (lavapipe, WARP) renders, but it is the answer
        // to "why is this slow"; `ok` would hide the finding.
        let software = report.gpu.device_type.as_deref() == Some("cpu");
        Check::new(
            "gpu",
            if software { Status::Warn } else { Status::Ok },
            format!(
                "{} ({} backend, {}{})",
                report.gpu.name.as_deref().unwrap_or("adapter"),
                report.gpu.backend.as_deref().unwrap_or("unknown"),
                report.gpu.device_type.as_deref().unwrap_or("unknown"),
                driver_suffix(&report.gpu),
            ) + if software {
                " — a software rasterizer: felis renders, slowly"
            } else {
                ""
            },
        )
    } else {
        Check::new(
            "gpu",
            Status::Fail,
            "wgpu found no GPU adapter; the window will not open. \
             Check your graphics driver installation",
        )
    };

    let clipboard = if report.clipboard.available {
        Check::new("clipboard", Status::Ok, "OS clipboard reachable")
    } else {
        Check::new(
            "clipboard",
            Status::Warn,
            format!(
                "OS clipboard unavailable ({}); felis falls back to its \
                 in-process clipboard, so copy/paste works inside felis only",
                report.clipboard.detail.as_deref().unwrap_or("no detail"),
            ),
        )
    };
    (gpu, clipboard)
}

fn driver_suffix(gpu: &felis_client_core::doctor::GpuProbe) -> String {
    let driver = [&gpu.driver, &gpu.driver_info]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" ");
    if driver.is_empty() {
        driver
    } else {
        format!(", driver {driver}")
    }
}

/// The `Err` string is a checklist detail, not a diagnostic: nothing
/// here aborts the run.
fn run_probe(program: &OsStr, flag: &OsStr) -> Result<ProbeReport, String> {
    run_probe_classified(program, flag).map_err(|failure| failure.message)
}

struct ProbeFailure {
    /// clap's refusal of an argument this frontend does not know: exit
    /// `2` naming it on stderr.
    unknown_flag: bool,
    message: String,
}

fn run_probe_classified(program: &OsStr, flag: &OsStr) -> Result<ProbeReport, ProbeFailure> {
    let failed = |message: String| ProbeFailure {
        unknown_flag: false,
        message,
    };
    let output = Command::new(program).arg(flag).output().map_err(|err| {
        failed(format!(
            "the felis GUI frontend ({}) could not be run: {err}",
            program.to_string_lossy()
        ))
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ProbeFailure {
            unknown_flag: output.status.code() == Some(2) && stderr.contains("unexpected argument"),
            message: format!(
                "the felis GUI frontend exited {} for {}; it may predate this check",
                output.status,
                flag.to_string_lossy(),
            ),
        });
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let report: ProbeReport = serde_json::from_str(text.trim())
        .map_err(|err| failed(format!("the frontend's probe report did not parse: {err}")))?;
    if report.v != PROBE_VERSION {
        // Refuse rather than read fields out of an unknown epoch: a
        // mismatched pair on `$PATH` is the install to report, not to
        // quietly reinterpret.
        return Err(failed(format!(
            "the frontend speaks probe version {} and this build reads {PROBE_VERSION}; \
             the two binaries are from different releases",
            report.v
        )));
    }
    Ok(report)
}

/// Whatever stops the font probe, the plain one is asked next so the
/// `gpu` and `clipboard` rows do not regress; the note says why the
/// report then carries no fonts.
fn run_report_probe(
    program: &OsStr,
    config: &ConfigSource,
) -> (Result<ProbeReport, String>, Option<String>) {
    let mut flag = OsString::from(DOCTOR_REPORT_PROBE_FLAG);
    if let ConfigSource::Explicit(path) = config {
        flag.push("=");
        flag.push(path);
    }
    let failure = match run_probe_classified(program, &flag) {
        Ok(report) => return (Ok(report), None),
        Err(failure) => failure,
    };
    let plain = run_probe(program, DOCTOR_PROBE_FLAG.as_ref());
    let note = plain.is_ok().then(|| {
        if failure.unknown_flag {
            "this felis-client predates the font probe".to_owned()
        } else {
            failure.message
        }
    });
    (plain, note)
}

/// A warning, never a failure: a purely local felis never invokes
/// `ssh`.
fn remote_helper_check() -> Check {
    match which_on_path("ssh") {
        Some(path) => Check::new(
            "remote_helper",
            Status::Ok,
            format!("ssh at {}", path.display()),
        ),
        None => Check::new(
            "remote_helper",
            Status::Warn,
            "no `ssh` on PATH; `felis --host` and `felis ssh` cannot connect \
             (local sessions are unaffected)",
        ),
    }
}

/// Hand-rolled rather than shelling out to `which`/`where`: the check
/// must answer the same way on a machine missing those too.
fn which_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    std::env::split_paths(&path)
        .map(|dir| dir.join(&file))
        .find(|candidate| candidate.is_file())
}

/// A socket parent must be a `0700` directory this uid owns (REQ-107),
/// and `TempDir` follows the process umask.
#[cfg(all(test, unix))]
fn private_dir() -> tempfile::TempDir {
    #![allow(clippy::unwrap_used)]
    use std::os::unix::fs::PermissionsExt as _;

    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    tmp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(version: &str, revision: char) -> BuildIdentity {
        BuildIdentity::from_build_env(version, &revision.to_string().repeat(40))
    }

    #[test]
    fn a_daemon_on_the_installed_build_draws_no_note() {
        let running = identity("0.2.0", 'a');
        assert_eq!(installed_skew(&running, Some(&running.clone())), None);
        assert_eq!(
            installed_skew(&running, None),
            None,
            "no installed binary to compare"
        );
    }

    #[test]
    fn a_daemon_on_another_build_names_the_installed_one() {
        let note =
            installed_skew(&identity("0.1.2", 'a'), Some(&identity("0.2.0", 'b'))).expect("a note");
        assert!(note.contains("0.2.0 (bbbbbbbbbbbb)"), "{note}");
    }

    /// The status tokens are the machine contract.
    #[test]
    fn status_tokens_are_the_documented_set() {
        assert_eq!(Status::Ok.token(), "ok");
        assert_eq!(Status::Warn.token(), "warn");
        assert_eq!(Status::Fail.token(), "fail");
        assert_eq!(Status::Skipped.token(), "skipped");
    }

    /// A stand-in frontend: `script` is the body of a `sh` program that
    /// sees the probe flag as `$1`.
    #[cfg(unix)]
    fn fake_frontend(dir: &Path, script: &str) -> OsString {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join("felis-client");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.into_os_string()
    }

    #[cfg(unix)]
    const PLAIN_REPORT: &str = r#"{"v":1,"client_version":"0.1.0","gpu":{"available":true},"clipboard":{"available":true}}"#;

    /// An older frontend refuses the font probe the way clap refuses
    /// any unknown flag; the plain probe still fills the `gpu` and
    /// `clipboard` rows.
    #[cfg(unix)]
    #[test]
    fn a_frontend_that_predates_the_font_probe_still_answers_the_plain_one() {
        let dir = private_dir();
        let program = fake_frontend(
            dir.path(),
            &format!(
                "[ \"$1\" = --doctor-probe ] || {{ echo \"error: unexpected argument '$1' found\" >&2; exit 2; }}\necho '{PLAIN_REPORT}'"
            ),
        );
        let (probe, note) = run_report_probe(&program, &ConfigSource::Default);
        let report = probe.unwrap();
        assert_eq!(report.fonts, None);
        assert!(note.unwrap().contains("predates"));
        let (gpu, clipboard) = probe_rows(&Ok(report));
        assert_eq!(gpu.status, Status::Ok);
        assert_eq!(clipboard.status, Status::Ok);
    }

    /// A current frontend whose font probe fails keeps that failure as
    /// the reason, rather than being blamed on its age.
    #[cfg(unix)]
    #[test]
    fn a_failing_font_probe_keeps_its_own_reason() {
        let dir = private_dir();
        let program = fake_frontend(
            dir.path(),
            &format!("[ \"$1\" = --doctor-probe ] || exit 101\necho '{PLAIN_REPORT}'"),
        );
        let (probe, note) = run_report_probe(&program, &ConfigSource::Default);
        assert!(probe.is_ok());
        let note = note.unwrap();
        assert!(!note.contains("predates the font probe"), "{note}");
        assert!(note.contains("101"), "{note}");
    }

    /// The selected config reaches the frontend as the flag's value, so
    /// fonts resolve against the file the rest of the report read.
    #[cfg(unix)]
    #[test]
    fn the_font_probe_names_the_selected_config() {
        let dir = private_dir();
        let config = dir.path().join("work.toml");
        let program = fake_frontend(
            dir.path(),
            &format!(
                "[ \"$1\" = --doctor-report-probe={} ] || exit 2\necho '{}'",
                config.display(),
                PLAIN_REPORT.replace(
                    r#""clipboard":{"available":true}"#,
                    r#""clipboard":{"available":true},"fonts":{"error":"no font"}"#
                ),
            ),
        );
        let (probe, note) = run_report_probe(&program, &ConfigSource::Explicit(config));
        assert_eq!(note, None);
        assert_eq!(
            probe.unwrap().fonts,
            Some(felis_client_core::doctor::FontsProbe::Failed {
                error: "no font".to_owned()
            })
        );
    }

    /// A headless install has no GUI binary: both probe rows come back
    /// `skipped`, and the run does not panic on the missing child.
    #[test]
    fn an_absent_frontend_skips_both_probe_rows() {
        let (gpu, clipboard) = probe_rows(&run_probe(
            "felis-client-that-is-not-there".as_ref(),
            DOCTOR_PROBE_FLAG.as_ref(),
        ));
        assert_eq!(gpu.status, Status::Skipped);
        assert_eq!(clipboard.status, Status::Skipped);
        assert!(gpu.detail.contains("could not be run"), "{}", gpu.detail);
    }

    /// A frontend from another release is refused, not reinterpreted.
    #[test]
    fn a_probe_from_a_different_epoch_is_refused() {
        // `sh -c` is no stand-in for the frontend; drive the parse
        // directly.
        let text = format!(
            r#"{{"v":{},"client_version":"9.9.9","gpu":{{"available":true}},"clipboard":{{"available":true}}}}"#,
            PROBE_VERSION + 1
        );
        let report: ProbeReport = serde_json::from_str(&text).unwrap();
        assert_ne!(report.v, PROBE_VERSION);
    }

    #[test]
    fn the_config_row_points_at_the_config_verb() {
        // Whatever this machine's config is, the row is one of the
        // four shapes.
        let check = config_check(&ConfigSource::Default);
        assert_eq!(check.name, "config");
        if matches!(check.status, Status::Warn | Status::Fail)
            && check.detail.contains("config.toml")
        {
            assert!(
                check.detail.contains("felis config check"),
                "{}",
                check.detail
            );
        }
    }

    #[test]
    fn the_remote_helper_row_is_never_fatal() {
        assert_ne!(remote_helper_check().status, Status::Fail);
    }

    /// A daemon that accepts a major this build never offered is a
    /// `fail` row naming both majors, in one line of prose: a `warn`
    /// would read as "not running", and a wrapped literal would leak
    /// its indentation into the table and the JSON `detail`.
    #[cfg(unix)]
    #[test]
    fn the_daemon_row_fails_on_an_accept_naming_an_unoffered_major() {
        use felis_client_core::{Carrier, Offer};
        use felis_protocol::preface::{DaemonPreface, PROTOCOL_MAJOR, PROTOCOL_MINOR};
        use felis_transport::Endpoint;
        use felis_transport::local::Listener;
        use felis_transport::preface::write_daemon_preface;
        use tokio::io::AsyncReadExt as _;

        let tmp = private_dir();
        let path = tmp.path().join("wrong-accept.sock");

        let (bound_tx, bound_rx) = std::sync::mpsc::channel();
        let listen_path = path.clone();
        let server = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = Listener::bind(&Endpoint::unix(listen_path)).expect("bind");
                bound_tx.send(()).expect("doctor still waiting");
                let stream = listener.accept().await.expect("accept");
                let (mut read, mut write) = stream.into_split();
                write_daemon_preface(
                    &mut write,
                    DaemonPreface::Accept {
                        major: PROTOCOL_MAJOR + 9,
                        minor: PROTOCOL_MINOR,
                    },
                )
                .await
                .expect("write accept");
                drop(read.read(&mut [0_u8; 1]).await);
            });
        });

        bound_rx.recv().expect("fake daemon bound");
        let target = Reconnector {
            carrier: Carrier::Local(Endpoint::unix(path)),
            offer: Offer::ops(),
        };
        let runtime = crate::build_runtime().expect("runtime");
        let (check, live) = runtime.block_on(daemon_check(&target, None));
        server.join().expect("fake daemon thread");

        assert_eq!(check.name, "daemon");
        assert_eq!(check.status, Status::Fail);
        assert_eq!(live, Primary::Live, "a daemon that answered is running");
        assert_eq!(
            check.detail,
            format!(
                "running, but accepted protocol major {} when this build offered {PROTOCOL_MAJOR}: \
                 the daemon is not speaking felis's negotiation, rebuild both halves",
                PROTOCOL_MAJOR + 9
            ),
        );
        assert!(
            !check.detail.contains("  "),
            "the detail must be one line of prose, got {:?}",
            check.detail
        );
    }

    /// A refused connect is classified by what the endpoint path holds:
    /// only a socket inode is the stale socket a stopped daemon leaves,
    /// and the next start replaces it. Anything else is named, because
    /// the next start refuses it instead.
    #[cfg(unix)]
    #[test]
    fn a_refused_endpoint_is_classified_by_what_the_path_holds() {
        let tmp = private_dir();
        let refused =
            ConnectError::Connect(std::io::Error::from(std::io::ErrorKind::ConnectionRefused));

        let socket = tmp.path().join("daemon.sock");
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        assert_eq!(
            not_running(&refused, Some(&socket)).detail,
            "not running (stale socket, replaced on the next start); a window launch starts one",
        );

        let directory = tmp.path().join("a-directory");
        std::fs::create_dir(&directory).unwrap();
        let symlink = tmp.path().join("a-symlink");
        std::os::unix::fs::symlink(&socket, &symlink).unwrap();
        let regular = tmp.path().join("a-regular-file");
        std::fs::write(&regular, b"").unwrap();
        for (path, what) in [
            (&directory, "a directory"),
            (&symlink, "a symlink"),
            (&regular, "a regular file"),
        ] {
            assert_eq!(
                not_running(&refused, Some(path)).detail,
                format!(
                    "not running ({refused}; {} is {what}); a window launch starts one",
                    path.display()
                ),
            );
        }

        let absent = tmp.path().join("absent");
        assert_eq!(
            not_running(&refused, Some(&absent)).detail,
            format!("not running ({refused}); a window launch starts one"),
        );
        let cold = ConnectError::Connect(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert_eq!(
            not_running(&cold, Some(&socket)).detail,
            format!("not running ({cold}); a window launch starts one"),
        );
    }
}

/// Unix-only: the endpoint partition, the sibling row, and the
/// provenance that gates both need real sockets.
#[cfg(all(test, unix))]
mod endpoint_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;
    use felis_client_core::{Carrier, Offer};
    use felis_protocol::messages::{ConnToClientMsg, RefusalReason};
    use felis_protocol::preface::{DaemonPreface, PROTOCOL_MAJOR, PROTOCOL_MINOR};
    use felis_transport::local::{Endpoint, Listener, ServerStream, server_split};
    use felis_transport::{
        FrameReader, FrameWriter,
        preface::{read_client_preface, write_daemon_preface},
    };

    /// What a listener does with the probe or the doctor's own dial.
    #[derive(Debug, Clone, Copy)]
    enum Fake {
        Silent,
        AcceptThenSilent,
        Welcoming,
        RefusingFrame(RefusalReason),
        UnknownStatus,
        RefusingMajor,
        /// Answers the version preface, then closes: an answer, but not
        /// a handshake.
        AcceptThenClose,
    }

    struct FakeDaemon {
        accepts: Arc<AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    impl FakeDaemon {
        fn start(path: &Path, behavior: Fake) -> Self {
            let listener = Listener::bind(&Endpoint::unix(path.to_path_buf())).expect("bind");
            let accepts = Arc::new(AtomicUsize::new(0));
            let counter = accepts.clone();
            let task = tokio::spawn(async move {
                while let Ok(stream) = listener.accept().await {
                    counter.fetch_add(1, Ordering::SeqCst);
                    tokio::spawn(serve_fake(stream, behavior));
                }
            });
            Self { accepts, task }
        }

        fn accepts(&self) -> usize {
            self.accepts.load(Ordering::SeqCst)
        }
    }

    impl Drop for FakeDaemon {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn serve_fake(stream: ServerStream, behavior: Fake) {
        let (read_half, mut write_half) = server_split(stream);
        let mut read_half = read_half;
        if matches!(behavior, Fake::Silent) {
            std::future::pending::<()>().await;
        }
        if read_client_preface(&mut read_half).await.is_err() {
            return;
        }
        let reply = match behavior {
            Fake::UnknownStatus => DaemonPreface::Unknown {
                status: 61,
                words: [3, 4],
            },
            Fake::RefusingMajor => DaemonPreface::Refuse {
                min_major: PROTOCOL_MAJOR + 1,
                max_major: PROTOCOL_MAJOR + 2,
            },
            _ => DaemonPreface::Accept {
                major: PROTOCOL_MAJOR,
                minor: PROTOCOL_MINOR,
            },
        };
        if write_daemon_preface(&mut write_half, reply).await.is_err() {
            return;
        }
        let mut reader = FrameReader::new(read_half);
        let mut writer = FrameWriter::at_build_minor(write_half);
        match behavior {
            Fake::Welcoming => {
                if reader.next_frame().await.is_err() {
                    return;
                }
                drop(
                    writer
                        .send(&ConnToClientMsg::Welcome { identity: None })
                        .await,
                );
            }
            Fake::RefusingFrame(reason) => {
                if reader.next_frame().await.is_err() {
                    return;
                }
                drop(
                    writer
                        .send(&ConnToClientMsg::Refused {
                            reason,
                            detail: "the daemon is at 0 of 0 connections".to_owned(),
                        })
                        .await,
                );
            }
            Fake::AcceptThenClose => return,
            Fake::Silent | Fake::AcceptThenSilent | Fake::UnknownStatus | Fake::RefusingMajor => {}
        }
        std::future::pending::<()>().await;
    }

    fn local_target(path: &Path) -> Reconnector {
        Reconnector {
            carrier: Carrier::Local(Endpoint::unix(path.to_path_buf())),
            offer: Offer::ops(),
        }
    }

    fn resolved(path: &Path, source: SocketSource) -> Resolved {
        Resolved {
            target: local_target(path),
            local_source: Some(source),
        }
    }

    /// `None` when the caller can read the directory anyway (root, or
    /// `CAP_DAC_OVERRIDE`), for which `EACCES` cannot be provoked.
    fn undialable_dir(parent: &Path, name: &str) -> Option<PathBuf> {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = parent.join(name);
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&dir).is_ok() {
            make_readable(&dir);
            return None;
        }
        Some(dir)
    }

    fn make_readable(dir: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    async fn report(
        source: SocketSource,
        target: &Path,
        default: &Path,
        primary: &Primary,
    ) -> EndpointReport {
        endpoint_report_from(source, target, Ok(default.to_path_buf()), primary).await
    }

    /// What the endpoint section reasons about at all: a local carrier,
    /// whatever named its address.
    #[test]
    fn only_a_local_carrier_has_an_endpoint_to_reason_about() {
        let path = Path::new("/tmp/felis.0/daemon.sock");
        for source in [
            SocketSource::Default,
            SocketSource::Stamped,
            SocketSource::Explicit,
        ] {
            assert_eq!(
                local_endpoint(&resolved(path, source)),
                Some((source, path.to_path_buf())),
                "{source:?}"
            );
        }
        assert_eq!(
            local_endpoint(&Resolved {
                target: Reconnector {
                    carrier: Carrier::Ssh {
                        destination: "user@host".to_owned(),
                        ssh_args: vec![],
                    },
                    offer: Offer::ops(),
                },
                local_source: None,
            }),
            None,
            "the SSH carrier has no local endpoint"
        );
    }

    /// A stamp equal to the default is the ordinary place to type
    /// `felis doctor`: inside a felis session of the current daemon,
    /// where there is no second endpoint to reason about.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stamp_equal_to_the_default_is_reported_like_the_default() {
        let dir = private_dir();
        let default = dir.path().join("default.sock");
        let _current = FakeDaemon::start(&default, Fake::Welcoming);

        let report = report(SocketSource::Stamped, &default, &default, &Primary::Live).await;

        assert!(report.note.is_none());
        assert!(report.rows.is_empty(), "{}", report.rows.len());
    }

    /// One socket reached by two names is one daemon, so a stamp that
    /// only spells the default differently is not a stranded one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stamp_that_aliases_the_default_is_not_stranded() {
        let dir = private_dir();
        let default = dir.path().join("default.sock");
        let _daemon = FakeDaemon::start(&default, Fake::Welcoming);
        let aliased = dir.path().join("link.sock");
        std::os::unix::fs::symlink(&default, &aliased).unwrap();

        let report = report(SocketSource::Stamped, &aliased, &default, &Primary::Live).await;

        assert!(report.note.is_none(), "{:?}", report.note);
        assert!(report.rows.is_empty(), "{}", report.rows.len());
    }

    /// The stamp a legacy daemon put in its shells points at an
    /// endpoint no process resolves any more: the note says so, and the
    /// row reports the daemon the rest of the host now reaches.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stamp_on_a_stranded_endpoint_is_noted_and_the_default_probed() {
        let dir = private_dir();
        let default = dir.path().join("default.sock");
        let stamped = dir.path().join("tmp.sock");
        let _current = FakeDaemon::start(&default, Fake::Welcoming);

        let report = report(SocketSource::Stamped, &stamped, &default, &Primary::Live).await;

        let note = report.note.expect("a stranded stamp is worth saying");
        assert!(note.contains("env -u FELIS_SOCKET"), "{note}");
        assert!(note.contains(&default.display().to_string()), "{note}");
        assert_eq!(report.rows.len(), 1);
        assert!(
            report.rows[0].detail.contains("also answers"),
            "{}",
            report.rows[0].detail
        );
    }

    /// Whether a daemon answers the stamp is the `daemon` row's own
    /// outcome, so the sibling row states it rather than assuming it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stranded_stamp_that_answered_nothing_says_so() {
        let dir = private_dir();
        let default = dir.path().join("default.sock");
        let stamped = dir.path().join("tmp.sock");
        let _current = FakeDaemon::start(&default, Fake::Welcoming);

        let report = report(SocketSource::Stamped, &stamped, &default, &Primary::Absent).await;

        let detail = &report.rows[0].detail;
        assert!(detail.contains("where no daemon answered"), "{detail}");
        assert!(detail.contains("nothing is listening there"), "{detail}");
        assert!(!detail.contains("also answers"), "{detail}");
    }

    /// A daemon that answers anything of its own is a daemon: a
    /// refusal, a preface status this build cannot name, and a major
    /// refusal each warn, while a listener that says nothing is
    /// reported without one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn only_what_a_daemon_answers_warns_on_the_sibling_row() {
        for (behavior, daemon) in [
            (Fake::Welcoming, true),
            (Fake::RefusingFrame(RefusalReason::AtCapacity), true),
            (Fake::UnknownStatus, true),
            (Fake::RefusingMajor, true),
            (Fake::Silent, false),
            (Fake::AcceptThenSilent, false),
        ] {
            let dir = private_dir();
            let default = dir.path().join("default.sock");
            let stamped = dir.path().join("stamped.sock");
            let _daemon = FakeDaemon::start(&default, behavior);

            let report = tokio::time::timeout(
                PROBE_DEADLINE * 3,
                report(SocketSource::Stamped, &stamped, &default, &Primary::Live),
            )
            .await
            .expect("every probe is bounded");

            assert_eq!(report.rows.len(), 1, "{behavior:?}");
            let row = &report.rows[0];
            if daemon {
                assert_eq!(row.status, Status::Warn, "{behavior:?} {}", row.detail);
                assert!(row.detail.contains("also answers"), "{}", row.detail);
            } else {
                assert_eq!(row.status, Status::Ok, "{behavior:?} {}", row.detail);
                assert!(!row.detail.contains("also answers"), "{}", row.detail);
            }
        }
    }

    /// The stranded reader's question is what is at the endpoint they
    /// are not on, and a cold one answers it as much as a live one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cold_default_is_reported_beside_a_stranded_stamp() {
        let dir = private_dir();
        let default = dir.path().join("default.sock");
        let stamped = dir.path().join("stamped.sock");

        let report = report(SocketSource::Stamped, &stamped, &default, &Primary::Live).await;

        assert!(report.note.is_some());
        assert_eq!(report.rows.len(), 1);
        let row = &report.rows[0];
        assert_eq!(row.name, "daemon-sibling");
        assert_eq!(row.status, Status::Ok, "{}", row.detail);
        assert!(row.detail.contains("is cold"), "{}", row.detail);
        assert!(
            row.detail.contains(&stamped.display().to_string())
                && row.detail.contains(&default.display().to_string()),
            "{}",
            row.detail
        );
    }

    /// An explicit `--socket` names one address on purpose, and a stamp
    /// pointing somewhere no environment resolves is a private endpoint.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_explicit_socket_and_an_unrelated_stamp_get_nothing() {
        let dir = private_dir();
        let default = dir.path().join("default.sock");
        let elsewhere = dir.path().join("elsewhere.sock");
        let live_default = FakeDaemon::start(&default, Fake::Welcoming);

        for (source, target) in [
            (SocketSource::Explicit, &default),
            (SocketSource::Explicit, &elsewhere),
        ] {
            let report = report(source, target, &default, &Primary::Live).await;
            assert!(report.note.is_none(), "{source:?} {:?}", report.note);
            assert!(report.rows.is_empty(), "{source:?}");
        }
        assert_eq!(live_default.accepts(), 0, "the default was not probed");

        let unresolvable = endpoint_report_from(
            SocketSource::Explicit,
            &default,
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the default endpoint is not a directory you own",
            )),
            &Primary::Live,
        )
        .await;
        assert!(
            unresolvable.note.is_none() && unresolvable.rows.is_empty(),
            "an address the caller named is dialed whatever the default resolves to"
        );
    }

    /// A default that cannot be resolved is the next command's failure,
    /// and the row that dialed the stamp still stands.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_default_that_cannot_be_resolved_is_named_on_the_daemon_row() {
        let dir = private_dir();
        let stamped = dir.path().join("stamped.sock");

        let report = endpoint_report_from(
            SocketSource::Stamped,
            &stamped,
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the default endpoint is not a directory you own",
            )),
            &Primary::Live,
        )
        .await;

        let note = report.note.expect("the resolution failure is a finding");
        assert!(
            note.starts_with("the default endpoint cannot be resolved"),
            "{note}"
        );
        assert!(note.contains("not a directory you own"), "{note}");
        assert!(report.rows.is_empty());
    }

    /// Today's rendering for a cold socket, which the partition must
    /// keep: this is the normal state before the first window.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cold_default_endpoint_still_reads_not_running() {
        let dir = private_dir();
        let path = dir.path().join("cold.sock");
        let (row, live) = daemon_check(&local_target(&path), Some(&path)).await;
        assert_eq!(row.status, Status::Warn);
        assert!(row.detail.contains("not running"), "{}", row.detail);
        assert_ne!(live, Primary::Live);
    }

    /// A listener that accepts and says nothing is not a cold socket,
    /// and must not hold `doctor` either. The connect completed, so the
    /// expiry is the handshake's and the row says what was reached.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_silent_listener_on_the_default_endpoint_is_bounded_and_unverified() {
        let dir = private_dir();
        let path = dir.path().join("silent.sock");
        let _listener = FakeDaemon::start(&path, Fake::Silent);

        let (row, live) = tokio::time::timeout(
            PROBE_DEADLINE * 3,
            daemon_check(&local_target(&path), Some(&path)),
        )
        .await
        .expect("the default-endpoint dial is bounded");

        assert_eq!(row.status, Status::Warn);
        assert!(
            row.detail.starts_with(&format!(
                "something listens at {} but did not answer as a felis daemon",
                path.display()
            )),
            "{}",
            row.detail
        );
        assert!(!row.detail.contains("not running"), "{}", row.detail);
        assert!(matches!(live, Primary::Unverified { .. }), "{live:?}");
    }

    /// A peer that answers the version preface and then closes has
    /// answered something, so it is unverified rather than cold.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_peer_that_drops_mid_handshake_is_unverified_not_absent() {
        let dir = private_dir();
        let path = dir.path().join("half.sock");
        let _listener = FakeDaemon::start(&path, Fake::AcceptThenClose);

        let (row, live) = daemon_check(&local_target(&path), Some(&path)).await;

        assert_eq!(row.status, Status::Warn);
        assert!(
            row.detail.contains("did not answer as a felis daemon"),
            "{}",
            row.detail
        );
        assert!(!row.detail.contains("not running"), "{}", row.detail);
        assert!(matches!(live, Primary::Unverified { .. }), "{live:?}");
    }

    /// `EACCES` says nothing about whether a daemon is there, so the row
    /// names the path and the error instead of asserting absence.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_undialable_default_endpoint_names_the_path() {
        let dir = private_dir();
        let Some(closed) = undialable_dir(dir.path(), "closed") else {
            return;
        };
        let path = closed.join("daemon.sock");

        let (row, live) = daemon_check(&local_target(&path), Some(&path)).await;
        assert_eq!(row.status, Status::Warn);
        assert!(
            row.detail
                .starts_with(&format!("could not dial {}", path.display())),
            "{}",
            row.detail
        );
        assert!(!row.detail.contains("not running"), "{}", row.detail);
        assert_ne!(live, Primary::Live);
        make_readable(&closed);
    }

    /// The deadline belongs to the default endpoint alone: an explicit
    /// `--socket` keeps today's unbounded dial.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_explicit_socket_keeps_its_unbounded_dial() {
        let dir = private_dir();
        let path = dir.path().join("silent.sock");
        let _listener = FakeDaemon::start(&path, Fake::Silent);

        assert!(
            tokio::time::timeout(
                PROBE_DEADLINE + Duration::from_millis(500),
                daemon_check(&local_target(&path), None),
            )
            .await
            .is_err(),
            "an unbounded dial is still waiting after the probe deadline"
        );
    }

    /// Answers from a daemon, not dial failures: these render on every
    /// provenance and carrier.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_daemon_that_answers_is_never_reported_as_not_running() {
        for (behavior, expected) in [
            (Fake::UnknownStatus, "running, but answered preface status"),
            (
                Fake::RefusingFrame(RefusalReason::AtCapacity),
                "running, but refused this connection",
            ),
            (
                Fake::RefusingFrame(RefusalReason::Role),
                "running, but refused this connection",
            ),
        ] {
            let dir = private_dir();
            let path = dir.path().join("daemon.sock");
            let _daemon = FakeDaemon::start(&path, behavior);

            for bounded in [Some(path.as_path()), None] {
                let (row, live) = daemon_check(&local_target(&path), bounded).await;
                assert_eq!(row.status, Status::Warn, "{}", row.detail);
                assert!(row.detail.starts_with(expected), "{}", row.detail);
                assert!(!row.detail.contains("not running"), "{}", row.detail);
                assert_eq!(live, Primary::Live, "{}", row.detail);
            }
        }
    }
}

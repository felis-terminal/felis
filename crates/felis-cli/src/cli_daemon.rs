//! `felis daemon status` (what the running daemon is, and what it is
//! holding against its admission ceilings) and `felis daemon stop`,
//! the portable way to end it.

#![expect(
    clippy::print_stdout,
    reason = "this module renders the human framing of a report"
)]

use anyhow::Result;
use clap::Subcommand;
use felis_client_core::{Connection, DaemonStatus, Reconnector};
use felis_protocol::messages::{
    Limit, ReportScope, ResourceKind, ResourceReport, ResourceUnit, StopMode, StopOutcome,
    SubjectKind,
};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::cli_output::{
    DaemonStatusResult, DaemonStopResult, ErrorKind, Format, PointFormat, ProtocolVersion,
    Reporter, ResourceObject,
};
use crate::conn::Dial;

#[derive(Debug, Subcommand)]
pub(crate) enum DaemonOp {
    /// Report the running daemon: build, wire version, and accounted resources.
    ///
    /// Surfaces observed usage against admission limits. Never starts a
    /// daemon: exits 2 if none is running.
    Status {
        #[command(flatten)]
        output: PointFormat,
    },
    /// Stop the running daemon over IPC.
    ///
    /// Refuses while sessions remain, reporting how many; `--force`
    /// destroys them and `--when-empty` waits them out. Never starts a
    /// daemon.
    Stop {
        /// Destroy every session, then stop.
        #[arg(long)]
        force: bool,
        /// Refuse new sessions and stop once the last one ends.
        #[arg(long, conflicts_with = "force")]
        when_empty: bool,
        #[command(flatten)]
        output: PointFormat,
    },
}

impl DaemonOp {
    pub(crate) const fn format(&self) -> Format {
        match self {
            Self::Status { output } | Self::Stop { output, .. } => output.format,
        }
    }
}

/// Never autospawn: both verbs speak about a daemon that is running,
/// and starting one in order to stop it is the clearest case of it.
pub(crate) const DIAL: Dial = Dial::Ops;

pub(crate) fn run(
    runtime: &tokio::runtime::Runtime,
    op: DaemonOp,
    target: &Reconnector,
) -> Result<i32> {
    let format = op.format();
    runtime.block_on(async move {
        let out = Reporter::point(format);
        let conn = match DIAL.open(target, &out).await {
            Ok(conn) => conn,
            Err(code) => return Ok(code),
        };
        match op {
            DaemonOp::Status { .. } => cmd_status(conn, &out).await,
            DaemonOp::Stop {
                force, when_empty, ..
            } => cmd_stop(conn, &out, stop_mode(force, when_empty)).await,
        }
    })
}

/// clap refuses the pair, so the two flags are three postures.
const fn stop_mode(force: bool, when_empty: bool) -> StopMode {
    if force {
        StopMode::Force
    } else if when_empty {
        StopMode::WhenEmpty
    } else {
        StopMode::IfEmpty
    }
}

async fn cmd_stop<R, W>(mut conn: Connection<R, W>, out: &Reporter, mode: StopMode) -> Result<i32>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let outcome = match conn.daemon_stop(mode).await {
        Ok(outcome) => outcome,
        // Exit 2: a daemon too old to answer must be stopped the way
        // its own generation was (`docs/how-to/update-felis.md`).
        Err(err) => return Ok(out.fail(ErrorKind::Unsupported, err)),
    };
    if let StopOutcome::Refused { sessions } = outcome {
        // A refusal, not a report: the daemon is still running and the
        // caller's request did not happen.
        let message = refusal_message(sessions);
        return Ok(if out.machine() {
            out.fail_refusal(sessions, message)
        } else {
            out.fail(ErrorKind::Refused, message)
        });
    }
    if out.machine() {
        out.result(&stop_result(mode, outcome));
    } else {
        println!("{}", stop_message(mode, outcome));
    }
    Ok(0)
}

fn refusal_message(sessions: u32) -> String {
    format!(
        "the daemon holds {sessions} session(s) and was not stopped; \
         stop it with --force to destroy them, or --when-empty to wait them out"
    )
}

/// The mode is named beside the outcome, human and machine alike: a
/// reader must be able to tell a stop that destroyed sessions from one
/// that found none.
fn stop_message(mode: StopMode, outcome: StopOutcome) -> String {
    match (mode, outcome) {
        (StopMode::Force, _) => "destroyed every session; the daemon is stopping".to_owned(),
        (_, StopOutcome::Stopping) => "the daemon held no session; it is stopping".to_owned(),
        (_, StopOutcome::Draining { sessions }) => format!(
            "the daemon is draining: it refuses new sessions and exits \
             after the last of {sessions} ends"
        ),
        // Answered before this renderer is reached.
        (_, StopOutcome::Refused { sessions }) => refusal_message(sessions),
    }
}

const fn stop_result(mode: StopMode, outcome: StopOutcome) -> DaemonStopResult {
    DaemonStopResult {
        outcome: match outcome {
            StopOutcome::Stopping => "stopping",
            StopOutcome::Refused { .. } => "refused",
            StopOutcome::Draining { .. } => "draining",
        },
        mode: match mode {
            StopMode::IfEmpty => "if_empty",
            StopMode::Force => "force",
            StopMode::WhenEmpty => "when_empty",
        },
        sessions: match outcome {
            StopOutcome::Stopping => 0,
            StopOutcome::Refused { sessions } | StopOutcome::Draining { sessions } => sessions,
        },
    }
}

async fn cmd_status<R, W>(mut conn: Connection<R, W>, out: &Reporter) -> Result<i32>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let status = match conn.daemon_status().await {
        Ok(status) => status,
        // Exit 2, not 1: a daemon too old to answer is the same class
        // as an unreachable one; no retry or argument changes it.
        Err(err) => return Ok(out.fail(ErrorKind::Unsupported, err)),
    };
    if out.machine() {
        out.result(&status_result(&status));
    } else {
        print_status(&status);
    }
    Ok(0)
}

fn status_result(status: &DaemonStatus) -> DaemonStatusResult {
    DaemonStatusResult {
        version: status.version.clone(),
        protocol: ProtocolVersion {
            major: status.protocol_major,
            minor: status.protocol_minor,
        },
        worker_threads: status.worker_threads,
        draining: status.draining,
        resources: status.resources.iter().map(resource_object).collect(),
    }
}

/// The `v1` object is flat, so the scope arm is spread back over its
/// keys: an unlimited ceiling is an omitted key, and a daemon row
/// carries neither per-subject key.
fn resource_object(r: &ResourceReport) -> ResourceObject {
    let (scope, max_subject_used, per_subject_limit, global_limit) = match r.scope {
        ReportScope::Daemon { global_limit } => ("daemon", None, None, global_limit.bound()),
        ReportScope::Subject {
            subject,
            max_subject_used,
            per_subject_limit,
            global_limit,
        } => (
            subject_token(subject),
            Some(max_subject_used),
            per_subject_limit.bound(),
            global_limit.bound(),
        ),
    };
    ResourceObject {
        resource: resource_token(r.resource).to_owned(),
        unit: unit_token(r.unit).to_owned(),
        scope: scope.to_owned(),
        total_used: r.total_used,
        max_subject_used,
        per_subject_limit,
        global_limit,
    }
}

/// The machine token doubles as the human label, so a `jq` filter and
/// a screenshot name the same thing.
const fn resource_token(kind: ResourceKind) -> &'static str {
    match kind {
        ResourceKind::Connections => "connections",
        ResourceKind::Sessions => "sessions",
        ResourceKind::ImageStoreBytes => "image_store_bytes",
        ResourceKind::InFlightDecodes => "in_flight_decodes",
        ResourceKind::InFlightDecodeBytes => "in_flight_decode_bytes",
        ResourceKind::SubscriberQueueBytes => "subscriber_queue_bytes",
        ResourceKind::PtyInputBytes => "pty_input_bytes",
    }
}

const fn unit_token(unit: ResourceUnit) -> &'static str {
    match unit {
        ResourceUnit::Count => "count",
        ResourceUnit::Bytes => "bytes",
    }
}

const fn subject_token(subject: SubjectKind) -> &'static str {
    match subject {
        SubjectKind::Session => "session",
        SubjectKind::Subscriber => "subscriber",
    }
}

/// The scope is printed on every row, not only the surprising ones: a
/// reader who must notice an *absent* qualifier has been given a trap.
fn print_status(status: &DaemonStatus) {
    println!("version:  {}", status.version);
    println!(
        "wire:     {}.{}",
        status.protocol_major, status.protocol_minor
    );
    // Not a resource row: the runtime is built at a fixed size, so
    // usage and ceiling would show the same number.
    println!("workers:  {}", status.worker_threads);
    // Printed on every status, not only a draining one: a reader who
    // must notice an *absent* line has been given a trap.
    println!("draining: {}", if status.draining { "yes" } else { "no" });
    let width = status
        .resources
        .iter()
        .map(|r| resource_token(r.resource).len())
        .max()
        .unwrap_or(0);
    for r in &status.resources {
        println!("{:<width$}  {}", resource_token(r.resource), render_row(r));
    }
}

/// The value column of one row. Every `/` here divides two numbers in
/// one denominator: a total against the daemon-wide budget, a subject
/// against the per-subject ceiling. A total over a per-subject limit
/// is the ratio this renderer exists to never print.
fn render_row(r: &ResourceReport) -> String {
    let total = render_amount(r.total_used, r.unit);
    let ceiling = |limit: Limit, per: &str| match limit.bound() {
        Some(bound) => format!("{} per {per}", render_amount(bound, r.unit)),
        // Not "0": unbounded differs from bounded at nothing.
        None => "unlimited".to_owned(),
    };
    let (subject, max_subject_used, per_subject_limit, global_limit) = match r.scope {
        // The daemon is its own single subject, so a second column
        // would print the same number twice.
        ReportScope::Daemon { global_limit } => {
            return format!("{total}  /  {}", ceiling(global_limit, "daemon"));
        }
        ReportScope::Subject {
            subject,
            max_subject_used,
            per_subject_limit,
            global_limit,
        } => (
            subject_token(subject),
            max_subject_used,
            per_subject_limit,
            global_limit,
        ),
    };
    let head = match global_limit {
        Limit::Bounded(_) => format!("total {total}  /  {}", ceiling(global_limit, "daemon")),
        // A bare observation rather than "unlimited": the row's ceiling
        // is the per-subject one beside it, and a reader offered two
        // "unlimited"s has to work out which of them binds.
        Limit::Unlimited => format!("total {total}"),
    };
    let deepest = render_amount(max_subject_used, r.unit);
    format!(
        "{head}  \u{00b7}  max {subject} {deepest}  /  {}",
        ceiling(per_subject_limit, subject)
    )
}

/// Byte counts get a binary suffix (256 MiB reads where 268435456 has
/// to be divided); counts stay bare.
fn render_amount(value: u64, unit: ResourceUnit) -> String {
    match unit {
        ResourceUnit::Count => value.to_string(),
        ResourceUnit::Bytes => render_bytes(value),
    }
}

fn render_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes;
    let mut idx = 0;
    // Exact steps only: 1.5 MiB reported as "1 MiB" would understate a
    // number the reader compares against a cap.
    while idx + 1 < UNITS.len() && value >= 1024 && value.is_multiple_of(1024) {
        value /= 1024;
        idx += 1;
    }
    format!("{value} {}", UNITS[idx])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mode rides beside the outcome, so a reader can tell a stop
    /// that destroyed sessions from one that found none.
    #[test]
    fn the_stop_object_names_the_mode_beside_the_outcome() {
        for (mode, outcome, want) in [
            (
                StopMode::IfEmpty,
                StopOutcome::Stopping,
                serde_json::json!({"outcome": "stopping", "mode": "if_empty", "sessions": 0}),
            ),
            (
                StopMode::Force,
                StopOutcome::Stopping,
                serde_json::json!({"outcome": "stopping", "mode": "force", "sessions": 0}),
            ),
            (
                StopMode::WhenEmpty,
                StopOutcome::Draining { sessions: 3 },
                serde_json::json!({"outcome": "draining", "mode": "when_empty", "sessions": 3}),
            ),
        ] {
            assert_eq!(
                serde_json::to_value(stop_result(mode, outcome)).unwrap(),
                want
            );
        }
        assert!(
            stop_message(StopMode::Force, StopOutcome::Stopping)
                .contains("destroyed every session"),
            "the human line states the destruction --force asked for"
        );
        assert!(
            stop_message(StopMode::IfEmpty, StopOutcome::Stopping).contains("held no session"),
            "and states that a default stop found nothing to destroy"
        );
    }

    /// The tokens are the machine contract; a wire-enum rename must not
    /// retype a row key.
    #[test]
    fn every_resource_has_its_documented_token() {
        for (kind, token) in [
            (ResourceKind::Connections, "connections"),
            (ResourceKind::Sessions, "sessions"),
            (ResourceKind::ImageStoreBytes, "image_store_bytes"),
            (ResourceKind::InFlightDecodes, "in_flight_decodes"),
            (ResourceKind::InFlightDecodeBytes, "in_flight_decode_bytes"),
            (ResourceKind::SubscriberQueueBytes, "subscriber_queue_bytes"),
            (ResourceKind::PtyInputBytes, "pty_input_bytes"),
        ] {
            assert_eq!(resource_token(kind), token);
        }
        assert_eq!(subject_token(SubjectKind::Session), "session");
        assert_eq!(subject_token(SubjectKind::Subscriber), "subscriber");
        assert_eq!(unit_token(ResourceUnit::Count), "count");
        assert_eq!(unit_token(ResourceUnit::Bytes), "bytes");
    }

    #[test]
    fn byte_amounts_round_only_when_exact() {
        assert_eq!(render_bytes(0), "0 B");
        assert_eq!(render_bytes(1023), "1023 B");
        assert_eq!(render_bytes(1024), "1 KiB");
        assert_eq!(render_bytes(256 * 1024 * 1024), "256 MiB");
        assert_eq!(render_bytes(1024 * 1024 + 1), "1048577 B");
        assert_eq!(render_amount(7, ResourceUnit::Count), "7");
    }

    const fn daemon_row(
        resource: ResourceKind,
        unit: ResourceUnit,
        total_used: u64,
        global_limit: Limit,
    ) -> ResourceReport {
        ResourceReport {
            resource,
            unit,
            total_used,
            scope: ReportScope::Daemon { global_limit },
        }
    }

    const fn subject_row(
        resource: ResourceKind,
        unit: ResourceUnit,
        subject: SubjectKind,
        total_used: u64,
        max_subject_used: u64,
        per_subject_limit: Limit,
        global_limit: Limit,
    ) -> ResourceReport {
        ResourceReport {
            resource,
            unit,
            total_used,
            scope: ReportScope::Subject {
                subject,
                max_subject_used,
                per_subject_limit,
                global_limit,
            },
        }
    }

    /// The golden object shape, one row per subject scope: a daemon row
    /// carries no subject dimensions, and an absent ceiling is an
    /// omitted key rather than a null.
    #[test]
    fn the_result_object_carries_the_dimensions_that_apply() {
        let status = DaemonStatus {
            version: "0.1.0 (abc1234)".into(),
            protocol_major: 1,
            protocol_minor: 7,
            worker_threads: 8,
            draining: false,
            resources: vec![
                daemon_row(
                    ResourceKind::Sessions,
                    ResourceUnit::Count,
                    2,
                    Limit::Bounded(256),
                ),
                subject_row(
                    ResourceKind::ImageStoreBytes,
                    ResourceUnit::Bytes,
                    SubjectKind::Session,
                    3072,
                    2048,
                    Limit::Bounded(268_435_456),
                    Limit::Unlimited,
                ),
                subject_row(
                    ResourceKind::SubscriberQueueBytes,
                    ResourceUnit::Bytes,
                    SubjectKind::Subscriber,
                    0,
                    0,
                    Limit::Unlimited,
                    Limit::Unlimited,
                ),
            ],
        };
        let rendered = serde_json::to_value(status_result(&status)).unwrap();
        assert_eq!(
            rendered["resources"],
            serde_json::json!([
                {
                    "resource": "sessions",
                    "unit": "count",
                    "scope": "daemon",
                    "total_used": 2,
                    "global_limit": 256,
                },
                {
                    "resource": "image_store_bytes",
                    "unit": "bytes",
                    "scope": "session",
                    "total_used": 3072,
                    "max_subject_used": 2048,
                    "per_subject_limit": 268_435_456,
                },
                {
                    "resource": "subscriber_queue_bytes",
                    "unit": "bytes",
                    "scope": "subscriber",
                    "total_used": 0,
                    "max_subject_used": 0,
                },
            ]),
        );
        assert_eq!(rendered["worker_threads"], 8);
        assert_eq!(rendered["draining"], false);
        assert_eq!(rendered["protocol"]["minor"], 7);
    }

    /// The rendered ratio a reader is most likely to misread: a
    /// daemon-wide sum over a ceiling charged per session. Every `/`
    /// must divide two numbers of the same scope.
    #[test]
    fn no_rendered_ratio_crosses_scopes() {
        let per_session = subject_row(
            ResourceKind::ImageStoreBytes,
            ResourceUnit::Bytes,
            SubjectKind::Session,
            3 * 1024 * 1024,
            2 * 1024 * 1024,
            Limit::Bounded(256 * 1024 * 1024),
            Limit::Unlimited,
        );
        let line = render_row(&per_session);
        assert_eq!(
            line,
            "total 3 MiB  \u{00b7}  max session 2 MiB  /  256 MiB per session"
        );
        let (total_part, _) = line.split_once('\u{00b7}').expect("two segments");
        assert!(
            !total_part.contains('/'),
            "the daemon-wide total has no per-session denominator: {line}"
        );

        let subscriber = render_row(&subject_row(
            ResourceKind::SubscriberQueueBytes,
            ResourceUnit::Bytes,
            SubjectKind::Subscriber,
            1024,
            1024,
            Limit::Unlimited,
            Limit::Unlimited,
        ));
        assert_eq!(
            subscriber,
            "total 1 KiB  \u{00b7}  max subscriber 1 KiB  /  unlimited"
        );
    }

    /// A daemon-scope row is its own subject, so it renders as the one
    /// ratio it has; an absent ceiling still says so.
    #[test]
    fn a_daemon_row_renders_one_ratio() {
        assert_eq!(
            render_row(&daemon_row(
                ResourceKind::Sessions,
                ResourceUnit::Count,
                2,
                Limit::Bounded(256),
            )),
            "2  /  256 per daemon"
        );
        assert_eq!(
            render_row(&daemon_row(
                ResourceKind::InFlightDecodes,
                ResourceUnit::Count,
                0,
                Limit::Unlimited,
            )),
            "0  /  unlimited"
        );
    }

    /// A daemon-wide budget beside a per-subject cap gives the total a
    /// denominator of its own; neither ratio borrows the other's.
    #[test]
    fn a_global_budget_gives_the_total_its_own_denominator() {
        assert_eq!(
            render_row(&subject_row(
                ResourceKind::ImageStoreBytes,
                ResourceUnit::Bytes,
                SubjectKind::Session,
                300 * 1024 * 1024,
                120 * 1024 * 1024,
                Limit::Bounded(256 * 1024 * 1024),
                Limit::Bounded(1024 * 1024 * 1024),
            )),
            "total 300 MiB  /  1 GiB per daemon  \u{00b7}  max session 120 MiB  /  256 MiB per session"
        );
    }
}

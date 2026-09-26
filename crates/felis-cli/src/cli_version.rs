//! `felis version`: three-way build comparison between CLI, GUI client, and running daemon.
//!
//! The daemon row is read over the handshake because a rebuilt client can reattach to a running daemon.

use std::ffi::OsString;
use std::process::Command;

use felis_client_core::{ConnectError, Reconnector, RemoteSpawn};
use felis_protocol::BuildIdentity;
use serde::Serialize;

use crate::cli_output::{PointFormat, Reporter};

/// This binary's own build, abbreviated: `felis --version`'s output.
pub(crate) const SELF_VERSION_LINE: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("FELIS_BUILD_STAMP_SHORT"),
    ")"
);

fn self_identity() -> BuildIdentity {
    BuildIdentity::from_build_env(env!("CARGO_PKG_VERSION"), env!("FELIS_BUILD_STAMP"))
}

/// `status` is the token a consumer branches on; `detail` is the human
/// column when there is no identity to print in it.
struct Row {
    identity: Option<BuildIdentity>,
    status: &'static str,
    detail: Option<String>,
}

impl Row {
    const fn found(identity: BuildIdentity) -> Self {
        Self {
            identity: Some(identity),
            status: "ok",
            detail: None,
        }
    }

    fn missing(status: &'static str, detail: impl Into<Option<String>>) -> Self {
        Self {
            identity: None,
            status,
            detail: detail.into(),
        }
    }

    fn human(&self) -> String {
        self.identity.as_ref().map_or_else(
            || {
                self.detail
                    .clone()
                    .unwrap_or_else(|| self.status.replace('_', " "))
            },
            BuildIdentity::human,
        )
    }
}

#[derive(Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct VersionResult<'a> {
    pub(crate) cli: &'a BuildIdentity,
    pub(crate) client: Option<&'a BuildIdentity>,
    /// `ok`, `unavailable` (no such binary), or `unrecognized` (it ran
    /// but did not print a canonical identity line).
    pub(crate) client_status: &'a str,
    pub(crate) daemon: Option<&'a BuildIdentity>,
    /// `ok`, `not_running`, `at_capacity`, `incompatible`, or `untyped`
    /// (a daemon whose `Welcome` carried no identity).
    pub(crate) daemon_status: &'a str,
}

#[allow(clippy::print_stdout)] // the human table IS this verb's contract
pub(crate) fn run(
    runtime: &tokio::runtime::Runtime,
    output: &PointFormat,
    target: &Reconnector,
    client_program: impl FnOnce() -> OsString,
) -> i32 {
    let out = Reporter::point(output.format);
    let cli = self_identity();
    let client = client_row(client_program);
    let daemon = runtime.block_on(daemon_row(target));

    if out.machine() {
        out.result(&VersionResult {
            cli: &cli,
            client: client.identity.as_ref(),
            client_status: client.status,
            daemon: daemon.identity.as_ref(),
            daemon_status: daemon.status,
        });
    } else {
        println!("{:<6} {}", "cli", cli.human());
        println!("{:<6} {}", "client", client.human());
        println!("{:<6} {}", "daemon", daemon.human());
    }
    0
}

/// Asking the binary rather than assuming cli and client are one build
/// catches a mismatched install.
fn client_row(client_program: impl FnOnce() -> OsString) -> Row {
    let Ok(out) = Command::new(client_program()).arg("--version").output() else {
        // A missing binary is the headless build, not a failure.
        return Row::missing("unavailable", None);
    };
    if !out.status.success() {
        return Row::missing("unavailable", None);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // The status word, not the captured line: an unrecognized `--version`
    // is unbounded and may be multi-line, and echoing it would break the
    // one-row-per-process shape this table promises.
    identity_from_version_line(&text).map_or_else(|| Row::missing("unrecognized", None), Row::found)
}

/// clap prefixes the canonical line with the binary's own name; the
/// rest is exactly what [`BuildIdentity`]'s `FromStr` accepts.
fn identity_from_version_line(text: &str) -> Option<BuildIdentity> {
    let line = text.lines().next()?;
    let (_name, identity) = line.trim().split_once(' ')?;
    identity.parse().ok()
}

/// The build probe reports which daemon is there; a spawned one would
/// be the answer to a question nobody asked.
pub(crate) const REMOTE_SPAWN: RemoteSpawn = RemoteSpawn::Refuse;

async fn daemon_row(target: &Reconnector) -> Row {
    let probed = crate::conn::dial(target, REMOTE_SPAWN).await;
    // Every refusal handled below came from a daemon that answered:
    // folding one into "not running" would deny the peer is there at
    // all, which is the skew this report exists to expose.
    if let Err(ref err) = probed
        && err.at_capacity().is_some()
    {
        return Row::missing(
            "at_capacity",
            Some("at capacity, build unavailable until a peer disconnects".to_owned()),
        );
    }
    match probed {
        Ok(conn) => conn.daemon_identity.map_or_else(
            || {
                Row::missing(
                    "untyped",
                    Some("running, but the daemon reported no identity".to_owned()),
                )
            },
            Row::found,
        ),
        Err(ConnectError::MajorMismatch {
            client_major,
            daemon_min,
            daemon_max,
        }) => Row::missing(
            "incompatible",
            Some(format!(
                "incompatible: daemon speaks protocol major {daemon_min}-{daemon_max} (client {client_major})"
            )),
        ),
        // The daemon's own status word: inventing a major range from
        // ours would read as a contradiction.
        Err(ConnectError::UnknownPrefaceStatus {
            status,
            client_major,
            ..
        }) => Row::missing(
            "incompatible",
            Some(format!(
                "incompatible: daemon refused with preface status {status} (client {client_major})"
            )),
        ),
        Err(ConnectError::AcceptedUnofferedMajor { offered, accepted }) => Row::missing(
            "incompatible",
            Some(format!(
                "incompatible: daemon accepted protocol major {accepted} (client offered {offered})"
            )),
        ),
        Err(_) => Row::missing("not_running", None),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use felis_client_core::{Carrier, Offer};
    #[cfg(unix)]
    use felis_transport::Endpoint;

    use super::*;

    /// A socket parent must be a `0700` directory this uid owns
    /// (REQ-107), and `TempDir` follows the process umask.
    #[cfg(unix)]
    fn private_dir() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::TempDir::new().unwrap();

        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

        tmp
    }

    #[cfg(unix)]
    fn local_target(endpoint: Endpoint) -> Reconnector {
        Reconnector {
            carrier: Carrier::Local(endpoint),
            offer: Offer::ops(),
        }
    }

    #[cfg(unix)]
    fn block_on_daemon_row(target: &Reconnector) -> Row {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(daemon_row(target))
    }

    /// A cold socket reads as `not running`, never the transport's
    /// errno, and the read-side dial can never start a daemon.
    #[cfg(unix)]
    #[test]
    fn a_cold_socket_reads_as_not_running() {
        let tmp = private_dir();
        let target = local_target(Endpoint::unix(tmp.path().join("absent.sock")));
        let row = block_on_daemon_row(&target);
        assert_eq!(row.status, "not_running");
        assert_eq!(row.human(), "not running");
    }

    /// A daemon across a protocol-major break is running and is the
    /// stalest one this report can meet; it must not read as "not
    /// running".
    #[cfg(unix)]
    #[test]
    fn a_skewed_daemon_is_named_by_its_majors() {
        use felis_protocol::preface::{DaemonPreface, PROTOCOL_MAJOR};

        let (min, max) = (PROTOCOL_MAJOR + 1, PROTOCOL_MAJOR + 2);
        let row = against_fake_daemon(
            "skew.sock",
            DaemonPreface::Refuse {
                min_major: min,
                max_major: max,
            },
        );
        assert_eq!(row.status, "incompatible");
        assert_eq!(
            row.human(),
            format!(
                "incompatible: daemon speaks protocol major {min}-{max} (client {PROTOCOL_MAJOR})"
            ),
        );
    }

    /// A refusal this build cannot name carries exactly one fact, the
    /// status word; the row must not invent a major range around it.
    #[cfg(unix)]
    #[test]
    fn a_refusal_this_build_cannot_name_reports_its_status_word() {
        use felis_protocol::preface::{DaemonPreface, PROTOCOL_MAJOR};

        let row = against_fake_daemon(
            "future.sock",
            DaemonPreface::Unknown {
                status: 7,
                words: [9, 9],
            },
        );
        assert_eq!(row.status, "incompatible");
        assert_eq!(
            row.human(),
            format!("incompatible: daemon refused with preface status 7 (client {PROTOCOL_MAJOR})"),
        );
    }

    /// A status-0 reply naming a foreign major is a running peer that
    /// failed negotiation; the row reports both majors rather than
    /// folding it into "not running".
    #[cfg(unix)]
    #[test]
    fn an_accept_naming_an_unoffered_major_reports_both() {
        use felis_protocol::preface::{DaemonPreface, PROTOCOL_MAJOR, PROTOCOL_MINOR};

        let row = against_fake_daemon(
            "wrong-accept.sock",
            DaemonPreface::Accept {
                major: PROTOCOL_MAJOR + 9,
                minor: PROTOCOL_MINOR,
            },
        );
        assert_eq!(row.status, "incompatible");
        assert_eq!(
            row.human(),
            format!(
                "incompatible: daemon accepted protocol major {} (client offered {PROTOCOL_MAJOR})",
                PROTOCOL_MAJOR + 9
            ),
        );
    }

    /// Runs a one-shot daemon that answers the preface with `answer`
    /// and holds the socket open until the probe drops it, so the
    /// refusal cannot race the close.
    #[cfg(unix)]
    fn against_fake_daemon(name: &str, answer: felis_protocol::preface::DaemonPreface) -> Row {
        use felis_transport::local::Listener;
        use felis_transport::preface::write_daemon_preface;
        use tokio::io::AsyncReadExt as _;

        let tmp = private_dir();
        let path = tmp.path().join(name);

        // The probe blocks on its own runtime, so the fake daemon needs
        // a thread and reactor of its own.
        let (bound_tx, bound_rx) = std::sync::mpsc::channel();
        let listen_path = path.clone();
        let server = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = Listener::bind(&Endpoint::unix(listen_path)).expect("bind");
                bound_tx.send(()).expect("probe still waiting");
                let stream = listener.accept().await.expect("accept");
                let (mut read, mut write) = stream.into_split();
                write_daemon_preface(&mut write, answer)
                    .await
                    .expect("write preface");
                drop(read.read(&mut [0_u8; 1]).await);
            });
        });

        bound_rx.recv().expect("fake daemon bound");
        let row = block_on_daemon_row(&local_target(Endpoint::unix(path)));
        server.join().expect("fake daemon thread");
        row
    }

    #[test]
    fn a_canonical_version_line_parses_back_into_an_identity() {
        let id = identity_from_version_line(
            "felis-client 0.1.0 (0123456789abcdef0123456789abcdef01234567-dirty)\n",
        )
        .unwrap();
        assert_eq!(id.version, "0.1.0");
        assert!(id.dirty);
    }

    /// Anything between parentheses is not an identity: a build that
    /// does not speak the canonical form must read as unrecognized.
    #[test]
    fn a_line_outside_the_canonical_form_yields_nothing() {
        assert!(identity_from_version_line("felis-client 0.1.0 (e3abf80)").is_none());
        assert!(identity_from_version_line("felis-client 0.1.0").is_none());
    }

    /// `--version`'s own line must be the abbreviated human rendering
    /// of the identity the verb reports, so the two surfaces cannot
    /// disagree about which build this is.
    #[test]
    fn the_self_report_line_matches_the_verb_s_own_row() {
        assert_eq!(SELF_VERSION_LINE, self_identity().human());
    }
}

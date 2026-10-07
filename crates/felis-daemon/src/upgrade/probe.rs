//! The successor answering, before anything stops and again over the
//! dump itself, whether it can take this daemon's place.

use std::{
    io::Write as _,
    os::fd::{AsFd as _, OwnedFd},
    path::Path,
    process::Stdio,
    time::Duration,
};

use felis_protocol::preface::{SUPPORTED_MAJOR_MAX, SUPPORTED_MAJOR_MIN};

use super::{Refusal, carrier, dump};

pub const EXIT_OK: i32 = 0;
pub const EXIT_DUMP_VERSION: i32 = 3;
pub const EXIT_MAJOR: i32 = 4;
pub const EXIT_DUMP_INVALID: i32 = 5;

const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// The successor's side: `felis-daemon upgrade-probe`. With
/// `check_dump`, the dump arrives on standard input and is decoded and
/// validated whole.
#[must_use]
pub fn answer(dump_version: u32, majors: (u16, u16), check_dump: bool) -> i32 {
    if !dump::reads_version(dump_version) {
        return refuse(
            EXIT_DUMP_VERSION,
            &format!(
                "this felis-daemon reads upgrade dump version {}, not {dump_version}",
                dump::DUMP_VERSION
            ),
        );
    }
    if majors.0 < SUPPORTED_MAJOR_MIN || majors.1 > SUPPORTED_MAJOR_MAX {
        return refuse(
            EXIT_MAJOR,
            &format!(
                "this felis-daemon serves protocol majors \
                 {SUPPORTED_MAJOR_MIN}-{SUPPORTED_MAJOR_MAX}, not all of {}-{}",
                majors.0, majors.1
            ),
        );
    }
    if check_dump {
        let stdin = std::io::stdin();
        let bytes = match carrier::read(stdin.as_fd()) {
            Ok(bytes) => bytes,
            Err(err) => {
                return refuse(
                    EXIT_DUMP_INVALID,
                    &format!("cannot read the upgrade dump: {err}"),
                );
            }
        };
        if let Err(err) = dump::Dump::decode(&bytes) {
            return refuse(EXIT_DUMP_INVALID, &err.to_string());
        }
    }
    EXIT_OK
}

/// The predecessor reads this line back as the refusal's detail.
fn refuse(code: i32, detail: &str) -> i32 {
    let _written = writeln!(std::io::stderr(), "{detail}");
    code
}

/// The predecessor's side. The majors asked for are every major this
/// daemon serves: any of them may be a connected client's.
pub async fn ask(successor: &Path, dump: Option<&OwnedFd>) -> Result<(), Refusal> {
    let mut cmd = tokio::process::Command::new(successor);
    cmd.arg("upgrade-probe")
        .arg("--dump-version")
        .arg(dump::DUMP_VERSION.to_string())
        .arg("--majors")
        .arg(format!("{SUPPORTED_MAJOR_MIN}-{SUPPORTED_MAJOR_MAX}"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match dump {
        Some(fd) => {
            cmd.arg("--check-dump")
                .stdin(carrier::probe_stdin(fd).map_err(|err| {
                    Refusal::ProbeFailed(format!("hand the dump to the successor: {err}"))
                })?);
        }
        None => {
            cmd.stdin(Stdio::null());
        }
    }
    let child = cmd
        .spawn()
        .map_err(|err| Refusal::ProbeFailed(format!("run {}: {err}", successor.display())))?;
    let output = tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_elapsed| {
            Refusal::Timeout(format!("{} did not answer the probe", successor.display()))
        })?
        .map_err(|err| Refusal::ProbeFailed(format!("wait for the probe: {err}")))?;
    let said = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    match output.status.code() {
        Some(EXIT_OK) => Ok(()),
        Some(EXIT_DUMP_VERSION) => Err(Refusal::DumpVersion(said)),
        Some(EXIT_MAJOR) => Err(Refusal::ProtocolMajor(said)),
        _ => Err(Refusal::ProbeFailed(if said.is_empty() {
            format!("the probe ended with {}", output.status)
        } else {
            said
        })),
    }
}

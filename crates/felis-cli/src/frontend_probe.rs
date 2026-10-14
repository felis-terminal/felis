//! One run of `felis-client` in a hidden probe mode, for the verbs that
//! need what only the frontend links (wgpu, the shaping stack) while
//! this binary stays GPU-free.

use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cause {
    /// No frontend binary at the resolved path or on `PATH`.
    Absent,
    /// clap's refusal of an argument this frontend does not know: exit
    /// `2` naming it on stderr.
    UnknownFlag,
    Failed,
}

#[derive(Debug)]
pub(crate) struct ProbeFailure {
    pub(crate) cause: Cause,
    pub(crate) message: String,
}

/// The frontend's stdout when it exits `0`.
pub(crate) fn run(program: &OsStr, flag: &OsStr) -> Result<String, ProbeFailure> {
    let output = Command::new(program)
        .arg(flag)
        .output()
        .map_err(|err| ProbeFailure {
            // An existing frontend whose interpreter or loader is missing
            // also fails with `NotFound`.
            cause: if err.kind() == std::io::ErrorKind::NotFound && !Path::new(program).is_file() {
                Cause::Absent
            } else {
                Cause::Failed
            },
            message: format!(
                "the felis GUI frontend ({}) could not be run: {err}",
                program.to_string_lossy()
            ),
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let unknown_flag =
            output.status.code() == Some(2) && stderr.contains("unexpected argument");
        return Err(ProbeFailure {
            cause: if unknown_flag {
                Cause::UnknownFlag
            } else {
                Cause::Failed
            },
            message: format!(
                "the felis GUI frontend exited {} for {}; it may predate this check",
                output.status,
                flag.to_string_lossy(),
            ),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

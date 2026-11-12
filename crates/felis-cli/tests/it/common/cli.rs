use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::Command;

pub(crate) fn cli_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_felis"))
}

/// `FELIS_SOCKET` outranks the platform default, so a run started inside
/// a felis window would otherwise address the developer's own daemon.
/// `RUST_LOG` outranks machine mode's `"off"` fallback directive, so a
/// developer or runner with it exported would otherwise put log lines
/// on the channel the one-object assertions count.
pub(crate) fn felis_command(bin: impl AsRef<OsStr>) -> Command {
    let mut cmd = Command::new(bin);
    cmd.env_remove("FELIS_SOCKET");
    cmd.env_remove("RUST_LOG");
    cmd
}

pub(crate) fn cli_command() -> Command {
    felis_command(cli_bin())
}

#[cfg(unix)]
pub(crate) fn cli_on(socket: impl AsRef<std::path::Path>) -> Command {
    let mut cmd = cli_command();
    cmd.arg("--socket").arg(socket.as_ref());
    cmd
}

#[cfg(unix)]
pub(crate) fn felis_out(socket: &std::path::Path, args: &[&str]) -> std::process::Output {
    cli_on(socket)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("run felis {}: {err}", args.join(" ")))
}

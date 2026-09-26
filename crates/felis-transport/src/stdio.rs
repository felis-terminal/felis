//! Cross-host carrier: a child process (`ssh user@host felis-daemon
//! relay`) whose stdin/stdout carries the IPC frame stream
//! (`docs/reference/ipc.md` "Cross-host carrier: SSH stdio").

use std::io;

use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::framing::{FrameReader, FrameWriter};

/// Dropping the session does not kill the child; dropping `writer`
/// closes the remote daemon's stdin, which is its clean-shutdown signal.
pub struct StdioSession {
    pub child: Child,
    pub reader: FrameReader<ChildStdout>,
    pub writer: FrameWriter<ChildStdin>,
}

/// `stderr` stays inherited so SSH prompts and daemon-side panics reach
/// the user's terminal.
pub fn spawn_command(mut cmd: Command) -> io::Result<StdioSession> {
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    let mut child = cmd.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("child stdout was not piped"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("child stdin was not piped"))?;
    Ok(StdioSession {
        child,
        reader: FrameReader::new(stdout),
        writer: FrameWriter::baseline(stdin),
    })
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use felis_protocol::frame::Frame;

    #[cfg(unix)]
    use super::*;

    // Unix-only: a byte-clean passthrough child on Windows needs a helper
    // binary (`cmd` translates bytes; PowerShell's redirected
    // `[Console]::OpenStandardInput().Read()` deadlocks).

    #[cfg(unix)]
    fn echo_back_command() -> Command {
        Command::new("cat")
    }

    #[cfg(unix)]
    fn emit_bytes_command(bytes: &[u8]) -> Command {
        use std::fmt::Write as _;
        let mut arg = String::with_capacity(bytes.len() * 4);
        for b in bytes {
            write!(arg, "\\x{b:02x}").unwrap();
        }
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!("printf '{arg}'"));
        cmd
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cat_round_trips_a_frame_through_stdio() {
        let mut session = spawn_command(echo_back_command()).expect("spawn echo child");

        let body = b"stdio-carrier";
        let frame = Frame { kind: 1, body };
        session.writer.write_frame(&frame).await.unwrap();
        session.writer.flush().await.unwrap();

        let got = session
            .reader
            .next_frame()
            .await
            .unwrap()
            .expect("frame echoes through the child");
        assert_eq!(got.kind, 1);
        assert_eq!(got.body, body[..]);

        drop(session.writer);
        while session.reader.next_frame().await.unwrap().is_some() {}
        let status = session.child.wait().await.unwrap();
        assert!(status.success(), "echo child exited non-zero: {status:?}");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shell_command_drives_the_carrier() {
        let response = Frame {
            kind: 0,
            body: b"ok!",
        };
        let response_bytes = response.encode().expect("a 3-byte body encodes");
        let mut session =
            spawn_command(emit_bytes_command(&response_bytes)).expect("spawn emitter");
        let got = session
            .reader
            .next_frame()
            .await
            .unwrap()
            .expect("frame from sh");
        assert_eq!(got.body, b"ok!"[..]);
        drop(session.writer);
        drop(session.reader);
        let status = session.child.wait().await.unwrap();
        assert!(status.success());
    }
}

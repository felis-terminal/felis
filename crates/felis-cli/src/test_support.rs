#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_protocol::messages::SessionInfo;

/// A socket parent must be a `0700` directory this uid owns
/// (REQ-107), and `TempDir` follows the process umask.
#[cfg(unix)]
pub(crate) fn private_dir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt as _;

    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    tmp
}

/// A stand-in `felis-client`: `script` is the body of a `sh` program
/// that sees the probe flag as `$1`.
#[cfg(unix)]
pub(crate) fn fake_frontend(dir: &std::path::Path, script: &str) -> std::ffi::OsString {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join("felis-client");
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.into_os_string()
}

/// A detached, untagged 24x80 session; tests override the fields they
/// exercise.
pub(crate) fn session_info(id: u128) -> SessionInfo {
    SessionInfo {
        id,
        dims: felis_protocol::messages::GridDims {
            rows: 24,
            cols: 80,
            pixel_w: 0,
            pixel_h: 0,
        },
        title: None,
        cwd: None,
        idle_seconds: None,
        tags: Vec::new(),
        last_notification: None,
        foreground: None,
        exited: false,
        last_exit_code: None,
        attachments: Vec::new(),
        sequence: std::num::NonZeroU64::MIN,
        program_status: Vec::new(),
    }
}

/// Answers the preface with `answer`, then holds the socket open until
/// the peer drops it so a refusal cannot race the close. Its own thread
/// and reactor, because the caller blocks on its runtime.
#[cfg(unix)]
pub(crate) fn one_shot_daemon(
    path: std::path::PathBuf,
    answer: felis_protocol::preface::DaemonPreface,
) -> std::thread::JoinHandle<()> {
    use felis_transport::Endpoint;
    use felis_transport::local::Listener;
    use felis_transport::preface::write_daemon_preface;
    use tokio::io::AsyncReadExt as _;

    let (bound_tx, bound_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = Listener::bind(&Endpoint::unix(path)).expect("bind");
            bound_tx.send(()).expect("the caller still waits");
            let stream = listener.accept().await.expect("accept");
            let (mut read, mut write) = stream.into_split();
            write_daemon_preface(&mut write, answer)
                .await
                .expect("write preface");
            drop(read.read(&mut [0_u8; 1]).await);
        });
    });
    bound_rx.recv().expect("fake daemon bound");
    server
}

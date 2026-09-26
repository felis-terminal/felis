//! The real `felis-daemon relay` binary bridging the stdio carrier to
//! a persistent daemon. Lives here rather than beside the connector
//! tests in `felis-client-core` because Cargo sets
//! `CARGO_BIN_EXE_felis-daemon` only for the binary's own crate.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Stdio;
use std::time::Duration;

use felis_protocol::{
    ConnectionMode,
    codec::{self},
    messages::{ConnToClientMsg, ConnToDaemonMsg},
    preface::{ClientPreface, DaemonPreface},
};
use felis_transport::{FrameReader, FrameWriter, StdioSession, spawn_command};

mod common;
use common::{daemon_bin, wait_connectable};

/// A socket parent must be a `0700` directory this uid owns (REQ-107),
/// and `TempDir` follows the process umask.
fn private_dir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt as _;

    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    tmp
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn felis_daemon_stdio_relay_bridges_to_the_persistent_daemon() {
    let bin = daemon_bin();
    let dir = private_dir();
    let socket = dir.path().join("daemon.sock");

    let mut daemon = tokio::process::Command::new(&bin)
        .arg("serve")
        .arg("--socket")
        .arg(&socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn persistent daemon");
    wait_connectable(&socket).await;

    let mut cmd = tokio::process::Command::new(&bin);
    cmd.arg("relay").arg("--socket").arg(&socket);
    let StdioSession { reader, writer, .. } =
        spawn_command(cmd).expect("spawn felis-daemon stdio relay");

    // `into_inner` is sound only because nothing has read a frame yet.
    let mut read_half = reader.into_inner();
    let mut write_half = writer.into_inner();
    felis_transport::preface::write_client_preface(&mut write_half, ClientPreface::CURRENT)
        .await
        .unwrap();
    let accepted = tokio::time::timeout(
        Duration::from_secs(5),
        felis_transport::preface::read_daemon_preface(&mut read_half),
    )
    .await
    .expect("daemon should answer the preface within 5 s")
    .unwrap();
    assert!(
        matches!(accepted, DaemonPreface::Accept { .. }),
        "the relay must reach an accepting daemon, got {accepted:?}"
    );

    let mut reader = FrameReader::new(read_half);
    let mut writer = FrameWriter::at_build_minor(write_half);
    writer
        .send(&ConnToDaemonMsg::Hello {
            mode: ConnectionMode::Window,
            pull_paced: false,
        })
        .await
        .unwrap();

    let reply = tokio::time::timeout(Duration::from_secs(5), reader.next_frame())
        .await
        .expect("daemon should reply within 5 s")
        .unwrap()
        .expect("welcome frame");
    match codec::decode::<ConnToClientMsg>(&reply.body).unwrap() {
        ConnToClientMsg::Welcome { .. } => {}
        other => panic!("expected Welcome, got {other:?}"),
    }

    drop(writer);
    drop(reader);
    daemon.start_kill().expect("signal persistent daemon");
}

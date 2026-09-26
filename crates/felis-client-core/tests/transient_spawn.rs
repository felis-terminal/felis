//! Tests `pipe` / `run` transient spawning end to end against a live daemon.
//!
//! Asserts arguments from [`felis_client_core::pipe::prepare_spawn`] survive
//! `SessionToDaemonMsg::Create` intact (`docs/explanation/data-model/scrollback.md`).

#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use felis_client_core::connector::{Offer, connect};
use felis_client_core::pipe::{Origin, prepare_spawn};
use felis_daemon::SessionPool;
use felis_daemon::serve::{DaemonCaps, serve_unix};
use felis_protocol::messages::{GridDims, RegionPosition};
use tempfile::TempDir;

/// A socket parent must be a `0700` directory this uid owns
/// (REQ-107), and `TempDir` follows the process umask.
fn private_dir() -> TempDir {
    let tmp = TempDir::new().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    tmp
}
use tokio::sync::Mutex;

async fn spawn_daemon(tmp: &TempDir) -> std::path::PathBuf {
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let server_path = path.clone();
    tokio::spawn(async move {
        drop(serve_unix(&server_path, DaemonCaps::default(), pool).await);
    });
    for _ in 0..200 {
        if path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(path.exists(), "daemon never bound its socket");
    path
}

async fn read_when_written(path: &std::path::Path) -> String {
    for _ in 0..300 {
        if let Ok(text) = std::fs::read_to_string(path)
            && text.contains("END")
        {
            return text;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the transient never wrote its report to {}", path.display());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_transient_carries_the_region_and_the_origin_context_to_the_child() {
    let tmp = private_dir();
    let socket = spawn_daemon(&tmp).await;
    let report = tmp.path().join("report.txt");

    // `sh -c '<script>' <arg>` binds `$0` to the region file `prepare_spawn` appends.
    let script = format!(
        "{{ printenv FELIS_ORIGIN_SESSION_ID; printenv FELIS_HOST; printenv FELIS_CWD; \
         printenv FELIS_INPUT_LINE_NUMBER; cat \"$0\"; echo END; }} > {}",
        report.display()
    );
    let argv = vec!["/bin/sh".to_owned(), "-c".to_owned(), script];

    let origin = Origin {
        session_id: 0x0123_4567_89ab_cdef,
        host: Some("user@devbox".to_owned()),
        osc7: Some("file://devbox/srv/app".to_owned()),
    };
    let prepared = prepare_spawn(
        Some(b"region line\n"),
        &argv,
        GridDims {
            rows: 24,
            cols: 80,
            pixel_w: 0,
            pixel_h: 0,
        },
        Some(RegionPosition {
            top_line: 42,
            cursor_line: 43,
            cursor_column: 7,
        }),
        &origin,
    )
    .unwrap();

    let mut conn = connect(&socket, Offer::ops()).await.unwrap();
    conn.create_with(prepared.args).await.unwrap();

    let text = read_when_written(&report).await;
    assert!(
        text.contains("0123456789abcdef"),
        "the origin session id must reach the child: {text}"
    );
    assert!(
        text.contains("user@devbox"),
        "the origin carrier must reach the child: {text}"
    );
    assert!(
        text.contains("file://devbox/srv/app"),
        "the origin's OSC 7 report must reach the child verbatim: {text}"
    );
    assert!(
        text.contains("42"),
        "the region anchor must reach the child: {text}"
    );
    assert!(
        text.contains("region line"),
        "the child must be able to read the region file: {text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_transient_reaches_the_child_with_the_argv_unchanged() {
    let tmp = private_dir();
    let socket = spawn_daemon(&tmp).await;
    let report = tmp.path().join("report.txt");

    let script = format!("{{ echo \"args=[$*]\"; echo END; }} > {}", report.display());
    let argv = vec!["/bin/sh".to_owned(), "-c".to_owned(), script];

    let prepared = prepare_spawn(
        None,
        &argv,
        GridDims {
            rows: 24,
            cols: 80,
            pixel_w: 0,
            pixel_h: 0,
        },
        None,
        &Origin::default(),
    )
    .unwrap();
    assert!(prepared.region.is_none());

    let mut conn = connect(&socket, Offer::ops()).await.unwrap();
    conn.create_with(prepared.args).await.unwrap();

    let text = read_when_written(&report).await;
    assert!(
        text.contains("args=[]"),
        "a regionless run must append nothing to the argv: {text}"
    );
}

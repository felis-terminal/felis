//! The real `felis-daemon` handing its sessions to a successor process
//! on Windows. Each test runs at Medium integrity, the only level whose
//! daemon uses the bundled console host that can hand a session over.

#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command as StdCommand, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use felis_client_core::{AttachIntent, Offer, ShadowScreen, connect};
use felis_protocol::messages::{SpawnArgs, UpgradeOutcome, UpgradeRefusal};
use felis_protocol::{MessageKind, codec, messages::GridMsg};
use tempfile::TempDir;

mod common;

use common::medium;

struct Daemon {
    /// The test's handle to the first daemon, released once it exits so
    /// that nothing but the daemons keeps it open.
    first: Option<Child>,
    serving: u32,
    pipe: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut first) = self.first.take() {
            first.kill().ok();
            first.wait().ok();
        }
        let mut pending = vec![self.serving];
        while let Some(pid) = pending.pop() {
            pending.extend(medium::daemon_children(pid));
            medium::terminate(pid);
        }
    }
}

/// Follows an accepted upgrade: waits until the serving daemon has
/// exited, then records the successor it started.
async fn follow_upgrade(daemon: &mut Daemon) {
    let predecessor = daemon.serving;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while medium::alive(predecessor) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the predecessor exits after the commit"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if let Some(mut first) = daemon.first.take() {
        first.wait().unwrap();
    }
    let successors = medium::daemon_children(predecessor);
    assert_eq!(successors.len(), 1, "one successor: {successors:?}");
    daemon.serving = successors[0];
}

fn unique_pipe() -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    PathBuf::from(format!(
        r"\\.\pipe\felis-test-upgrade-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn spawn_daemon(stand_in: Option<&str>) -> Daemon {
    let pipe = unique_pipe();
    let mut cmd = StdCommand::new(common::daemon_bin());
    cmd.args(["serve", "--socket"])
        .arg(&pipe)
        .env("RUST_LOG", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(mode) = stand_in {
        cmd.env("FELIS_STAND_IN", mode);
    }
    let first = cmd.spawn().expect("spawn felis-daemon");
    Daemon {
        serving: first.id(),
        first: Some(first),
        pipe,
    }
}

async fn wait_until_up(pipe: &Path) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(pipe)
            .is_ok()
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no daemon came up on {}",
            pipe.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
}

fn lines(path: &Path) -> usize {
    std::fs::read_to_string(path).map_or(0, |text| text.lines().count())
}

async fn wait_for_more_lines(path: &Path, than: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while lines(path) <= than {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the session's shell stopped writing {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A shell that appends a line to `ticks` every 50 ms for as long as it
/// lives.
fn ticker(ticks: &Path) -> SpawnArgs {
    SpawnArgs {
        command: "powershell.exe".to_owned(),
        args: vec![
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-Command".to_owned(),
            format!(
                "while ($true) {{ Add-Content -LiteralPath '{}' -Value tick; Start-Sleep -Milliseconds 50 }}",
                ticks.display()
            ),
        ],
        ..SpawnArgs::default()
    }
}

/// The successor is a new process serving the same session, whose shell
/// goes on running; the predecessor has exited. Clients still accept the
/// server once nothing but the daemons holds the first one, through a
/// second upgrade as well.
#[test]
fn an_upgrade_hands_the_running_session_to_a_new_daemon_process() {
    medium::run(
        "an_upgrade_hands_the_running_session_to_a_new_daemon_process",
        || {
            runtime().block_on(async {
                let tmp = TempDir::new().unwrap();
                let ticks = tmp.path().join("ticks");
                let mut daemon = spawn_daemon(None);
                wait_until_up(&daemon.pipe).await;

                let mut conn = connect(&daemon.pipe, Offer::ops()).await.unwrap();
                let info = conn.spawn_session(ticker(&ticks)).await.unwrap();
                wait_for_more_lines(&ticks, 0).await;

                for _ in 0..2 {
                    let mut conn = connect(&daemon.pipe, Offer::ops()).await.unwrap();
                    let outcome = conn
                        .daemon_upgrade(common::daemon_bin())
                        .await
                        .expect("the upgrade is answered");
                    assert_eq!(outcome, UpgradeOutcome::Upgrading);
                    drop(conn);
                    follow_upgrade(&mut daemon).await;

                    let mut after = connect(&daemon.pipe, Offer::ops())
                        .await
                        .expect("the client accepts the successor");
                    let sessions = after.list_sessions().await.unwrap();
                    assert_eq!(
                        sessions.iter().map(|s| s.id).collect::<Vec<_>>(),
                        vec![info.id],
                        "the successor serves the same session under the same id"
                    );
                    let before = lines(&ticks);
                    wait_for_more_lines(&ticks, before).await;
                }
            });
        },
    );
}

async fn rehydrated_screen(pipe: &Path, id: u128) -> ShadowScreen {
    let mut conn = connect(pipe, Offer::window(false)).await.unwrap();
    let info = conn.attach(id, AttachIntent::Deliberate).await.unwrap();
    let mut shadow = ShadowScreen::new(info.dims.rows, info.dims.cols);
    let mut began = false;
    let read = async {
        while let Some(frame) = conn.next_frame().await.unwrap() {
            if frame.kind != MessageKind::Grid.as_u16() {
                continue;
            }
            let msg = codec::decode::<GridMsg>(&frame.body).unwrap();
            began |= matches!(msg, GridMsg::RehydrateBegin);
            let ended = matches!(msg, GridMsg::RehydrateEnd);
            shadow.apply(&msg).unwrap();
            if began && ended {
                return;
            }
        }
        panic!("the attach closed before its rehydrate ended");
    };
    tokio::time::timeout(Duration::from_secs(10), read)
        .await
        .expect("the rehydrate burst ends");
    shadow
}

/// What the session printed before the upgrade is on its screen after.
#[test]
fn an_upgrade_keeps_the_screen_a_session_drew() {
    medium::run("an_upgrade_keeps_the_screen_a_session_drew", || {
        runtime().block_on(async {
            let mut daemon = spawn_daemon(None);
            wait_until_up(&daemon.pipe).await;
            let mut conn = connect(&daemon.pipe, Offer::ops()).await.unwrap();
            let info = conn
                .spawn_session(SpawnArgs {
                    command: "cmd.exe".to_owned(),
                    args: vec![
                        "/q".to_owned(),
                        "/k".to_owned(),
                        "echo drawn before the upgrade".to_owned(),
                    ],
                    ..SpawnArgs::default()
                })
                .await
                .unwrap();
            let shadow = rehydrated_screen(&daemon.pipe, info.id).await;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            let mut shadow = shadow;
            while !common::shadow_contains(&shadow, "drawn before the upgrade") {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the session never drew"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
                shadow = rehydrated_screen(&daemon.pipe, info.id).await;
            }

            let mut conn = connect(&daemon.pipe, Offer::ops()).await.unwrap();
            let outcome = conn.daemon_upgrade(common::daemon_bin()).await.unwrap();
            assert_eq!(outcome, UpgradeOutcome::Upgrading);
            drop(conn);
            follow_upgrade(&mut daemon).await;

            let shadow = rehydrated_screen(&daemon.pipe, info.id).await;
            assert!(
                common::shadow_contains(&shadow, "drawn before the upgrade"),
                "{:?}",
                common::shadow_rows(&shadow)
            );
        });
    });
}

/// Windows attached when the upgrade starts all re-attach at once and
/// all land on the successor: the pipe name never lapses, and a dial
/// that finds every instance busy is retried.
#[test]
fn attached_windows_reattach_through_the_hand_over_at_once() {
    medium::run(
        "attached_windows_reattach_through_the_hand_over_at_once",
        || {
            runtime().block_on(async {
                let mut daemon = spawn_daemon(None);
                wait_until_up(&daemon.pipe).await;
                let mut conn = connect(&daemon.pipe, Offer::ops()).await.unwrap();
                let info = conn
                    .spawn_session(SpawnArgs {
                        command: "cmd.exe".to_owned(),
                        args: vec!["/q".to_owned(), "/k".to_owned()],
                        ..SpawnArgs::default()
                    })
                    .await
                    .unwrap();
                let mut windows = Vec::new();
                for _ in 0..4 {
                    let mut window = connect(&daemon.pipe, Offer::window(false)).await.unwrap();
                    window
                        .attach(info.id, AttachIntent::Deliberate)
                        .await
                        .unwrap();
                    windows.push(window);
                }

                let mut conn = connect(&daemon.pipe, Offer::ops()).await.unwrap();
                let outcome = conn.daemon_upgrade(common::daemon_bin()).await.unwrap();
                assert_eq!(outcome, UpgradeOutcome::Upgrading);
                drop(conn);

                let reattached = windows.into_iter().map(|mut window| {
                    let pipe = daemon.pipe.clone();
                    tokio::spawn(async move {
                        // The transport drops with the predecessor.
                        while window.next_frame().await.ok().flatten().is_some() {}
                        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
                        loop {
                            let attached = async {
                                let mut conn = connect(&pipe, Offer::window(false)).await.ok()?;
                                conn.attach(info.id, AttachIntent::Deliberate).await.ok()
                            };
                            if attached.await.is_some() {
                                return;
                            }
                            assert!(
                                tokio::time::Instant::now() < deadline,
                                "a window could not re-attach"
                            );
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    })
                });
                for window in reattached.collect::<Vec<_>>() {
                    window.await.unwrap();
                }
                follow_upgrade(&mut daemon).await;
            });
        },
    );
}

fn stand_in() -> String {
    Path::new(&common::daemon_bin())
        .parent()
        .unwrap()
        .join("examples")
        .join("upgrade_stand_in.exe")
        .to_string_lossy()
        .into_owned()
}

/// A successor that fails the hand-over before it is ready, in each way
/// it can, leaves the daemon serving: the session's shell never stopped,
/// and the daemon admits sessions again.
fn a_failed_hand_over_leaves_the_daemon_serving(
    test: &str,
    mode: &'static str,
    expected: UpgradeRefusal,
    said: &'static str,
) {
    medium::run(test, || {
        runtime().block_on(async {
            let tmp = TempDir::new().unwrap();
            let ticks = tmp.path().join("ticks");
            let daemon = spawn_daemon(Some(mode));
            wait_until_up(&daemon.pipe).await;
            let mut conn = connect(&daemon.pipe, Offer::ops()).await.unwrap();
            let info = conn.spawn_session(ticker(&ticks)).await.unwrap();
            wait_for_more_lines(&ticks, 0).await;

            let mut conn = connect(&daemon.pipe, Offer::ops()).await.unwrap();
            let outcome = conn
                .daemon_upgrade(stand_in())
                .await
                .expect("the refusal is answered");
            assert!(
                matches!(
                    &outcome,
                    UpgradeOutcome::Refused { reason, detail }
                        if *reason == expected && detail.contains(said)
                ),
                "{outcome:?}"
            );
            assert!(medium::alive(daemon.serving), "the same daemon serves");
            assert_eq!(
                medium::daemon_children(daemon.serving),
                Vec::<u32>::new(),
                "no successor outlives the refusal"
            );

            let mut after = connect(&daemon.pipe, Offer::ops()).await.unwrap();
            let sessions = after.list_sessions().await.unwrap();
            assert_eq!(
                sessions.iter().map(|s| s.id).collect::<Vec<_>>(),
                vec![info.id]
            );
            let before = lines(&ticks);
            wait_for_more_lines(&ticks, before).await;
            let created = after
                .spawn_session(SpawnArgs::default())
                .await
                .expect("the daemon admits creates again");
            // A daemon killed while a console host is still starting
            // leaves that host and its shell running.
            after
                .destroy_session(felis_protocol::SessionHex(created.id).to_string())
                .await
                .unwrap();
        });
    });
}

#[test]
fn a_successor_that_exits_before_ready_leaves_the_daemon_serving() {
    a_failed_hand_over_leaves_the_daemon_serving(
        "a_successor_that_exits_before_ready_leaves_the_daemon_serving",
        "exit",
        UpgradeRefusal::ProbeFailed,
        "ended before it was ready",
    );
}

#[test]
fn a_successor_that_answers_out_of_turn_leaves_the_daemon_serving() {
    a_failed_hand_over_leaves_the_daemon_serving(
        "a_successor_that_answers_out_of_turn_leaves_the_daemon_serving",
        "garbage",
        UpgradeRefusal::ProbeFailed,
        "upgrade frame 127",
    );
}

#[test]
fn a_successor_that_never_answers_leaves_the_daemon_serving() {
    a_failed_hand_over_leaves_the_daemon_serving(
        "a_successor_that_never_answers_leaves_the_daemon_serving",
        "silent",
        UpgradeRefusal::Timeout,
        "did not take over in time",
    );
}

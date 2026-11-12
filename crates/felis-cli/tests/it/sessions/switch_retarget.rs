use felis_client_core::{Offer, connect};
use felis_protocol::{MessageKind, codec, messages::PushMsg};

use super::*;

type StampedDaemon = (
    tokio::task::JoinHandle<()>,
    std::path::PathBuf,
    Arc<Mutex<SessionPool>>,
    (&'static str, std::path::PathBuf),
);

/// `window retarget` rejects the global `--socket`, so the child
/// reaches this daemon through the stamp every shell inside a felis
/// session carries: no variable moves the default endpoint any more.
fn spawn_default_socket_daemon(tmp: &TempDir, factory: SessionFactory) -> StampedDaemon {
    let path = tmp.path().join("stamped").join("daemon.sock");
    let stamp = ("FELIS_SOCKET", path.clone());
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let serving = bind_unix(&path, DaemonCaps::default(), pool.clone(), factory)
        .expect("the test daemon binds its socket");
    let handle = tokio::spawn(async move { drop(serving.await) });
    (handle, path, pool, stamp)
}

/// Both framings claim queue admission only, so exit 0 cannot read as
/// "the window moved".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switch_pushes_reattach_and_reports_only_queue_admission() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory());
    let target = create_and_detach(&socket).await;
    // The window-mode connection is the one the daemon pushes to.
    let mut gui = connect(&socket, Offer::window(false)).await.unwrap();
    let from = gui.create_with(SpawnArgs::default()).await.unwrap().id;

    let out = cli_on(&socket)
        .args([
            "sessions",
            "switch",
            &format!("{target:032x}"),
            "--format",
            "json",
        ])
        .env("FELIS_SESSION_ID", format!("{from:032x}"))
        .output()
        .expect("run felis sessions switch");
    assert!(
        out.status.success(),
        "switch must exit 0: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let result = parse_point(&stdout, "sessions switch");
    assert_eq!(
        result["queued"], 1,
        "the reply reports queue admission for the one window; got: {stdout}",
    );

    let pushed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = gui
                .reader
                .next_frame()
                .await
                .expect("read frame")
                .expect("daemon closed before Reattach");
            if frame.kind == MessageKind::Push.as_u16()
                && let Ok(PushMsg::Reattach { id }) = codec::decode::<PushMsg>(&frame.body)
            {
                return id;
            }
        }
    })
    .await
    .expect("no Reattach push within 5s");
    assert_eq!(pushed, target, "push must name the resolved target");

    let human = cli_on(&socket)
        .args(["sessions", "switch", &format!("{target:032x}")])
        .env("FELIS_SESSION_ID", format!("{from:032x}"))
        .output()
        .expect("run felis sessions switch");
    assert!(
        human.status.success(),
        "switch must exit 0: stderr={}",
        String::from_utf8_lossy(&human.stderr),
    );
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(
        stdout.contains("queued on 1 window") && stdout.contains("not landed"),
        "the human line must report admission and deny landing; got: {stdout}",
    );

    drop(gui);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switch_with_no_attached_client_exits_one() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory());
    let from = create_and_detach(&socket).await;
    let target = create_and_detach(&socket).await;

    let out = felis_out(
        &socket,
        &[
            "sessions",
            "switch",
            &format!("{target:032x}"),
            "--from",
            &format!("{from:032x}"),
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "switch with nobody attached must exit 1: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("has no window to move"),
        "expected the no-input-owner message; got: {stderr}",
    );

    server.abort();
}

/// Ids are never reused, so a stale one can only mean "gone", never a
/// redirect to a surviving window.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switch_with_an_unknown_attachment_exits_one() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory());
    let from = create_and_detach(&socket).await;
    let target = create_and_detach(&socket).await;

    let out = felis_out(
        &socket,
        &[
            "sessions",
            "switch",
            &format!("{target:032x}"),
            "--from",
            &format!("{from:032x}"),
            "--attachment",
            "4096",
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "an unknown attachment must exit 1: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("attachment 4096 is no longer on session"),
        "expected the stale-attachment message; got: {stderr}",
    );

    server.abort();
}

/// The post-condition already holds, so no window is needed to push to,
/// and neither framing may claim a queue admission that did not happen.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_same_session_switch_exits_zero_with_nothing_queued() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory());
    // Parked: the shape the default scope would otherwise deny.
    let id = create_and_detach(&socket).await;

    let out = felis_out(
        &socket,
        &[
            "sessions",
            "switch",
            &format!("{id:032x}"),
            "--from",
            &format!("{id:032x}"),
            "--format",
            "json",
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "a same-session switch must exit 0: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let result = parse_point(&stdout, "sessions switch onto itself");
    assert_eq!(
        result["queued"], 0,
        "nothing was pushed, and the count must say so; got: {stdout}",
    );

    let human = felis_out(
        &socket,
        &[
            "sessions",
            "switch",
            &format!("{id:032x}"),
            "--from",
            &format!("{id:032x}"),
        ],
    );
    assert_eq!(
        human.status.code(),
        Some(0),
        "a same-session switch must exit 0: stderr={}",
        String::from_utf8_lossy(&human.stderr),
    );
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(
        stdout.contains(&format!("already on session {id:032x}"))
            && stdout.contains("nothing queued"),
        "the human line must say nothing was pushed; got: {stdout}",
    );

    server.abort();
}

/// Exit 0 claims queue admission alone: a window that takes the push
/// and then drops leaves the result standing with nothing attached to
/// the session it named.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_target_that_disconnects_after_queue_admission_keeps_the_switch_result() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory());
    let target = create_and_detach(&socket).await;
    let mut gui = connect(&socket, Offer::window(false)).await.unwrap();
    let from = gui.create_with(SpawnArgs::default()).await.unwrap().id;

    let out = cli_on(&socket)
        .args([
            "sessions",
            "switch",
            &format!("{target:032x}"),
            "--format",
            "json",
        ])
        .env("FELIS_SESSION_ID", format!("{from:032x}"))
        .output()
        .expect("run felis sessions switch");
    assert_eq!(
        out.status.code(),
        Some(0),
        "switch must exit 0: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let result = parse_point(&stdout, "sessions switch");
    assert_eq!(result["queued"], 1, "one outbox took the push: {stdout}");

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = gui
                .reader
                .next_frame()
                .await
                .expect("read frame")
                .expect("daemon closed before Reattach");
            if frame.kind == MessageKind::Push.as_u16()
                && matches!(
                    codec::decode::<PushMsg>(&frame.body),
                    Ok(PushMsg::Reattach { .. })
                )
            {
                return;
            }
        }
    })
    .await
    .expect("no Reattach push within 5s");
    // The window takes the push and goes away without attaching. Its
    // departure from `from` is the daemon's last word on it.
    drop(gui);
    let attachments_of = |id: u128| {
        let out = felis_out(
            &socket,
            &[
                "sessions",
                "info",
                &format!("{id:032x}"),
                "--format",
                "json",
            ],
        );
        parse_point(&String::from_utf8_lossy(&out.stdout), "sessions info")["attachments"].clone()
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while attachments_of(from) != serde_json::json!([]) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the window never left its session"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let info = felis_out(
        &socket,
        &[
            "sessions",
            "info",
            &format!("{target:032x}"),
            "--format",
            "json",
        ],
    );
    let info_stdout = String::from_utf8_lossy(&info.stdout);
    let info = parse_point(&info_stdout, "sessions info");
    assert_eq!(
        info["attachments"],
        serde_json::json!([]),
        "the landing never happened, and exit 0 never promised it would: {info_stdout}",
    );

    server.abort();
}

/// The retarget's landing runs against a daemon that holds no record of
/// the request, so `queued` is all the source daemon can report.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cross_daemon_retarget_reports_admission_without_landing() {
    let tmp = private_dir();
    let elsewhere = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory());
    let (other_server, other_socket, _other_pool) = spawn_daemon(&elsewhere, quiet_factory());
    let mut gui = connect(&socket, Offer::window(false)).await.unwrap();
    let from = gui.create_with(SpawnArgs::default()).await.unwrap().id;

    let out = cli_command()
        .args(["window", "retarget"])
        .arg(&other_socket)
        .args(["--format", "json"])
        .env("FELIS_SOCKET", &socket)
        .env("FELIS_SESSION_ID", format!("{from:032x}"))
        .output()
        .expect("run felis window retarget");
    assert_eq!(
        out.status.code(),
        Some(0),
        "a cross-daemon retarget must exit 0 on admission: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let result = parse_point(&stdout, "window retarget");
    assert_eq!(result["queued"], 1, "one outbox took the push: {stdout}");

    let target = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = gui
                .reader
                .next_frame()
                .await
                .expect("read frame")
                .expect("daemon closed before RetargetHost");
            if frame.kind == MessageKind::Push.as_u16()
                && let Ok(PushMsg::RetargetHost { target, .. }) =
                    codec::decode::<PushMsg>(&frame.body)
            {
                return target;
            }
        }
    })
    .await
    .expect("no RetargetHost push within 5s");
    assert_eq!(
        target.carrier,
        felis_protocol::messages::RetargetCarrier::LocalEndpoint(
            other_socket.display().to_string()
        ),
    );

    let roster = felis_out(&other_socket, &["sessions", "list", "--format", "json"]);
    let roster_stdout = String::from_utf8_lossy(&roster.stdout);
    let roster = parse_point(&roster_stdout, "sessions list");
    assert_eq!(
        roster["sessions"],
        serde_json::json!([]),
        "the target daemon holds nothing: the exit code reported admission on the source \
         daemon only: {roster_stdout}",
    );

    drop(gui);
    other_server.abort();
    server.abort();
}

/// The retarget's human line differs from the switch's: the window
/// re-dials rather than re-attaching on the same carrier.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retarget_human_line_says_queued_and_not_landed() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory());
    let mut gui = connect(&socket, Offer::window(false)).await.unwrap();
    let from = gui.create_with(SpawnArgs::default()).await.unwrap().id;

    let out = cli_command()
        .args(["ssh", "user@box"])
        .env("FELIS_SOCKET", &socket)
        .env("FELIS_SESSION_ID", format!("{from:032x}"))
        .output()
        .expect("run felis ssh");
    assert_eq!(
        out.status.code(),
        Some(0),
        "felis ssh must exit 0 on admission: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("queued on 1 window")
            && stdout.contains("not landed")
            && stdout.contains("re-dials"),
        "the human line must report admission and deny landing; got: {stdout}",
    );

    drop(gui);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn window_retarget_without_a_carrier_heads_for_the_local_daemon() {
    let tmp = private_dir();
    let (server, socket, _pool, (env_var, env_value)) =
        spawn_default_socket_daemon(&tmp, quiet_factory());
    let mut gui = connect(&socket, Offer::window(false)).await.unwrap();
    let from = gui.create_with(SpawnArgs::default()).await.unwrap().id;

    let out = cli_command()
        .args(["window", "retarget"])
        .env(env_var, &env_value)
        .env("FELIS_SESSION_ID", format!("{from:032x}"))
        .output()
        .expect("run felis window retarget");
    assert_eq!(
        out.status.code(),
        Some(0),
        "no carrier must mean the default local daemon: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );

    let target = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = gui
                .reader
                .next_frame()
                .await
                .expect("read frame")
                .expect("daemon closed before RetargetHost");
            if frame.kind == MessageKind::Push.as_u16()
                && let Ok(PushMsg::RetargetHost { target, .. }) =
                    codec::decode::<PushMsg>(&frame.body)
            {
                return target;
            }
        }
    })
    .await
    .expect("no RetargetHost push within 5s");
    assert_eq!(
        target.carrier,
        felis_protocol::messages::RetargetCarrier::DefaultLocal,
    );

    server.abort();
}

/// The verb refuses the global `--socket` and nothing else in a child
/// names the window's daemon, so on a non-default socket the stamp is
/// the only way there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_in_window_retarget_reaches_the_stamped_daemon() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory());
    let mut gui = connect(&socket, Offer::window(false)).await.unwrap();
    let from = gui.create_with(SpawnArgs::default()).await.unwrap().id;

    let out = cli_command()
        .args(["ssh", "user@box"])
        .env("FELIS_SOCKET", &socket)
        .env("FELIS_SESSION_ID", format!("{from:032x}"))
        .output()
        .expect("run felis ssh");
    assert_eq!(
        out.status.code(),
        Some(0),
        "the stamp must name the daemon this window's session lives on: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );

    let target = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = gui
                .reader
                .next_frame()
                .await
                .expect("read frame")
                .expect("daemon closed before RetargetHost");
            if frame.kind == MessageKind::Push.as_u16()
                && let Ok(PushMsg::RetargetHost { target, .. }) =
                    codec::decode::<PushMsg>(&frame.body)
            {
                return target;
            }
        }
    })
    .await
    .expect("no RetargetHost push within 5s");
    assert_eq!(
        target.carrier,
        felis_protocol::messages::RetargetCarrier::Ssh {
            destination: "user@box".to_string(),
            ssh_args: Vec::new(),
        },
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_explicit_socket_outranks_the_stamp() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory());

    let out = cli_on(&socket)
        .args(["sessions", "list"])
        .env("FELIS_SOCKET", tmp.path().join("no-daemon-here.sock"))
        .output()
        .expect("run felis sessions list");
    assert_eq!(
        out.status.code(),
        Some(0),
        "the flag must outrank the stamp: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );

    server.abort();
}

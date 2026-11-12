use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;

use super::*;

/// The hidden completion helper lists the daemon an explicit
/// `--socket` names (the shell overlays forward only that flag).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn complete_sessions_lists_the_local_socket_roster() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory());
    let id = create_and_detach(&socket).await;

    let out = cli_on(&socket)
        .arg("__complete-sessions")
        .output()
        .expect("run felis __complete-sessions");
    assert!(
        out.status.success(),
        "non-zero exit: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let id_hex = format!("{id:032x}");
    assert!(
        stdout
            .lines()
            .any(|l| l.starts_with(&format!("{id_hex}\t"))),
        "expected a `<id>\\t<description>` line for {id_hex}, got: {stdout:?}"
    );

    server.abort();
}

/// Verify completion is local-only: `--host` on the line exits 0 with no candidates
/// and never starts `ssh`. The child's default socket is a live daemon to ensure
/// an empty candidate list is deliberate rather than a missing socket fallback.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn complete_sessions_with_a_host_emits_nothing_and_never_runs_ssh() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory());
    create_and_detach(&socket).await;
    let marker = tmp.path().join("ssh-ran");
    let ssh = tmp.path().join("ssh");
    let mut f = std::fs::File::create(&ssh).unwrap();
    writeln!(f, "#!/bin/sh\ntouch '{}'", marker.display()).unwrap();
    drop(f);
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut path = tmp.path().as_os_str().to_owned();
    path.push(":");
    path.push(fixture_path());

    for args in [
        vec!["--host", "nowhere", "__complete-sessions"],
        vec![
            "--host",
            "nowhere",
            "--ssh-arg=-p",
            "--ssh-arg=2222",
            "__complete-sessions",
        ],
    ] {
        let out = cli_command()
            .env("PATH", &path)
            .env("FELIS_SOCKET", &socket)
            .args(&args)
            .output()
            .expect("run felis __complete-sessions");
        assert_eq!(
            out.status.code(),
            Some(0),
            "{args:?}: status={:?} stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stderr),
        );
        assert!(
            out.stdout.is_empty(),
            "{args:?} offered candidates: {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(!marker.exists(), "{args:?} spawned ssh");
    }

    server.abort();
}

/// The auto-spawn matrix as `ssh` sees it (docs/reference/cli.md
/// "Auto-spawning"): asserted on the literal relay argv, because the
/// verb's policy has to survive the whole dial. The `ssh` stand-in on
/// `PATH` records its arguments before exiting, so the command line is
/// on disk even though the handshake then fails.
#[test]
fn the_ssh_relay_command_asks_for_a_spawn_only_for_the_spawning_verb() {
    let tmp = private_dir();
    let calls = tmp.path().join("ssh-argv");
    let ssh = tmp.path().join("ssh");
    let mut f = std::fs::File::create(&ssh).unwrap();
    writeln!(
        f,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'",
        calls.display()
    )
    .unwrap();
    drop(f);
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut path = tmp.path().as_os_str().to_owned();
    path.push(":");
    path.push(fixture_path());

    for (verb, expected) in [
        (
            ["sessions", "list"],
            "-- nowhere felis-daemon relay --no-spawn",
        ),
        (["sessions", "spawn"], "-- nowhere felis-daemon relay"),
    ] {
        drop(std::fs::remove_file(&calls));
        let out = cli_command()
            .env("PATH", &path)
            .args(["--host", "nowhere"])
            .args(verb)
            .output()
            .expect("run felis over the ssh carrier");
        assert!(
            !out.status.success(),
            "{verb:?}: the stand-in answers no handshake, so the verb must fail"
        );
        let recorded = std::fs::read_to_string(&calls).unwrap_or_default();
        assert_eq!(
            recorded.lines().collect::<Vec<_>>(),
            [expected],
            "{verb:?} dialed the wrong relay command"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_prints_one_session_and_exits_zero() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory());
    let id = create_and_detach(&socket).await;

    let out = felis_out(&socket, &["sessions", "list"]);
    assert!(
        out.status.success(),
        "non-zero exit: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let id_hex = format!("{id:032x}");
    let short = &id_hex[..8];
    assert!(
        stdout.contains(short),
        "list output missing session prefix {short}: {stdout}",
    );

    let out_json = felis_out(&socket, &["sessions", "list", "--format", "json"]);
    assert!(
        out_json.status.success(),
        "sessions list --format json must exit 0 (the plain form above \
         is checked; a json run that prints and then exits non-zero \
         would pass a stdout-only oracle)",
    );
    let stdout_json = String::from_utf8_lossy(&out_json.stdout);
    let object = parse_point(&stdout_json, "sessions list");
    let sessions = object["sessions"]
        .as_array()
        .expect("the roster is one array in one object");
    assert_eq!(sessions.len(), 1, "{stdout_json}");
    let item = &sessions[0];
    assert_eq!(item["id"], id_hex.as_str(), "the full id is the machine id");
    assert_eq!(
        item["short_id"], short,
        "the display prefix rides beside the id: {stdout_json}"
    );
    assert_eq!(
        item["attachments"],
        serde_json::json!([]),
        "a parked session must still carry an iterable attachments array"
    );
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_against_cold_socket_exits_two_without_autospawn() {
    let tmp = private_dir();
    let socket = tmp.path().join("nonexistent.sock");
    let out = felis_out(&socket, &["sessions", "list"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "cold socket must exit 2; got {:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        !socket.exists(),
        "ops must not auto-spawn the daemon on a cold socket",
    );
}

/// Without it, "no daemon" and "nothing to say" are the same empty
/// stdout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_that_never_opens_still_emits_its_error_terminal() {
    let tmp = private_dir();
    let socket = tmp.path().join("nonexistent.sock");
    let out = felis_out(&socket, &["sessions", "capture", "1a", "--format", "jsonl"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "cold socket must exit 2; got {:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let objects = parse_jsonl(&stdout);
    assert_all_versioned(&objects, "sessions capture --format jsonl");
    assert_eq!(objects.len(), 1, "only the terminal: {stdout}");
    assert_eq!(objects[0]["event"], "error");
    assert_eq!(objects[0]["error"]["kind"], "daemon_unreachable");
    assert!(
        objects[0]["error"]["message"].is_string(),
        "the terminal carries a human message too: {stdout}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_point_verbs_failure_is_one_typed_error_on_stderr() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory());
    let out = felis_out(
        &socket,
        &["sessions", "info", "--format", "json", "deadbeef"],
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "an unresolvable prefix is a domain failure: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).trim().is_empty(),
        "a failed point verb writes no result to stdout",
    );
    let error = parse_point(&String::from_utf8_lossy(&out.stderr), "sessions info");
    assert_eq!(error["error"]["kind"], "no_match");
    assert!(error["error"]["message"].is_string());

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn info_prints_session_metadata() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory());
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");
    let prefix = &id_hex[..8];

    let out = felis_out(&socket, &["sessions", "info", prefix]);
    assert!(
        out.status.success(),
        "non-zero: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("id:"),
        "info output missing label: {stdout}"
    );
    assert!(
        stdout.contains(&id_hex),
        "info output missing full id: {stdout}",
    );
    assert!(stdout.contains("size:"), "info missing size label");

    server.abort();
}

/// A session created without a cwd lists the daemon's, though its
/// program never sends OSC 7.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_and_info_show_the_spawn_directory() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory());
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");
    let daemon_cwd = std::env::current_dir()
        .unwrap()
        .into_os_string()
        .into_string()
        .unwrap();

    let list = felis_out(&socket, &["sessions", "list", "--format", "json"]);
    assert!(list.status.success(), "{list:?}");
    let row = parse_point(&String::from_utf8_lossy(&list.stdout), "sessions list")["sessions"]
        .as_array()
        .expect("sessions is an array")
        .iter()
        .find(|row| row["id"] == id_hex.as_str())
        .cloned()
        .expect("the session is listed");
    assert_eq!(row["spawn_cwd"], daemon_cwd.as_str());
    assert!(row.get("cwd").is_none(), "no OSC 7 was sent: {row}");

    let info = cli_on(&socket)
        .args(["sessions", "info", &id_hex[..8]])
        .output()
        .expect("run felis sessions info");
    assert!(info.status.success(), "{info:?}");
    let stdout = String::from_utf8_lossy(&info.stdout);
    assert!(
        stdout.contains(&format!("spawned:  {daemon_cwd}\n")),
        "info names the spawn directory: {stdout}"
    );

    server.abort();
}

/// A program's OSC 7501 and OSC 9;4 reports reach both `sessions list
/// --format json` and the human `sessions info`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_and_info_show_program_status_records_and_progress() {
    let tmp = private_dir();
    let factory = sh_factory(
        "printf '\\033]7501;state=blocked:kind=question:progress=50:app=claude-code:msg=d2hpY2ggZmlsZT8=\\033\\\\\\033]9;4;2;71\\007'; read x",
    );
    let (server, socket, pool) = spawn_daemon(&tmp, factory);
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while pool
        .lock()
        .await
        .handle_cloned(felis_daemon::SessionId(id))
        .is_none_or(|handle| handle.meta_snapshot().terminal_progress.is_none())
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the report never reached the daemon"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let list = felis_out(&socket, &["sessions", "list", "--format", "json"]);
    assert!(list.status.success(), "{list:?}");
    let list_out = String::from_utf8_lossy(&list.stdout);
    assert!(
        list_out.contains(
            r#""program_status":[{"id":"","state":"blocked","kind":"question","progress":50,"app":"claude-code","msg":"which file?"}]"#
        ),
        "list --format json must carry the record: {list_out}",
    );
    assert!(
        list_out.contains(r#""terminal_progress":{"state":"error","value":71}"#),
        "list --format json must carry the progress: {list_out}",
    );

    let info = felis_out(&socket, &["sessions", "info", &id_hex]);
    assert!(info.status.success(), "{info:?}");
    let info_out = String::from_utf8_lossy(&info.stdout);
    assert!(
        info_out.contains("status:   (root) blocked question 50% [claude-code]: which file?\n"),
        "info must print the record: {info_out}",
    );
    assert!(
        info_out.contains("progress: error 71%\n"),
        "info must print the progress: {info_out}",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill_removes_session_and_second_call_exits_one() {
    let tmp = private_dir();
    let (server, socket, pool) = spawn_daemon(&tmp, quiet_factory());
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let out = felis_out(&socket, &["sessions", "kill", &id_hex]);
    assert!(
        out.status.success(),
        "first kill must succeed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while pool.lock().await.get(felis_daemon::SessionId(id)).is_some() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "session must be gone from the pool after kill",
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let out2 = felis_out(&socket, &["sessions", "kill", &id_hex]);
    assert_eq!(
        out2.status.code(),
        Some(1),
        "second kill on missing id must exit 1: status={:?} stderr={}",
        out2.status,
        String::from_utf8_lossy(&out2.stderr),
    );
    let stderr = String::from_utf8_lossy(&out2.stderr);
    assert!(
        stderr.contains("no session"),
        "missing-session stderr expected; got: {stderr}",
    );

    server.abort();
}

/// `sessions info` resolves and shortens through one `Ops::Info`, so
/// the documented always-present `short_id` must still be there, must
/// still be a prefix of the id, and must still be at least the floor
/// length.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn info_json_still_carries_a_short_id_without_a_roster_fetch() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory());
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let out = cli_on(&socket)
        .args(["sessions", "info", &id_hex[..8], "--format", "json"])
        .output()
        .expect("run felis sessions info --format json");
    assert!(
        out.status.success(),
        "non-zero: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let object = parse_point(&stdout, "sessions info");
    assert_eq!(object["id"], id_hex.as_str());
    let short = object["short_id"].as_str().expect("short_id is a string");
    assert!(short.len() >= 8, "short_id below the floor: {stdout}");
    assert!(
        id_hex.starts_with(short),
        "short_id must be a prefix of the id: {stdout}"
    );

    server.abort();
}

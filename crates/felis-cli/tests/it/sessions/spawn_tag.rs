use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_creates_session_and_id_is_addressable() {
    let tmp = private_dir();
    let (server, socket, pool) = spawn_daemon(&tmp, quiet_factory());

    let out = felis_out(
        &socket,
        &["sessions", "spawn", "--", "/bin/sh", "-c", "read x"],
    );
    assert!(
        out.status.success(),
        "spawn must succeed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let id_hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(
        id_hex.len(),
        32,
        "stdout must be a 32-char hex id; got `{id_hex}`",
    );

    let id = u128::from_str_radix(&id_hex, 16).expect("hex parse");
    assert!(
        pool.lock().await.get(felis_daemon::SessionId(id)).is_some(),
        "spawned session must be in the pool under the printed id",
    );

    let list = felis_out(&socket, &["sessions", "list"]);
    assert!(
        String::from_utf8_lossy(&list.stdout).contains(&id_hex[..8]),
        "list must include the spawned session's short id",
    );

    let kill = felis_out(&socket, &["sessions", "kill", &id_hex]);
    assert!(
        kill.status.success(),
        "kill of spawned session must succeed"
    );

    server.abort();
}

/// 3000 fits `u16` and 70000 does not; both must reach the daemon's
/// REQ-605a admission point rather than die at argument parsing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_with_out_of_range_rows_is_refused_by_the_daemon() {
    let tmp = private_dir();
    let (server, socket, pool) = spawn_daemon(&tmp, quiet_factory());

    for rows in ["3000", "70000"] {
        let out = felis_out(
            &socket,
            &["sessions", "spawn", "--rows", rows, "--cols", "80"],
        );
        assert_eq!(
            out.status.code(),
            Some(1),
            "--rows {rows} is a typed refusal the daemon answered, so exit 1: stderr={}",
            String::from_utf8_lossy(&out.stderr),
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(&format!(
                "rows {rows} is outside the supported range 1..=2048"
            )),
            "--rows {rows} must report the daemon's typed refusal; got `{stderr}`",
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).trim().is_empty(),
            "a refused spawn must print no session id",
        );
    }
    assert_eq!(
        pool.lock().await.ids().count(),
        0,
        "a refused spawn must not leave a session behind",
    );

    server.abort();
}

/// Geometry is a pair: a create asks for the daemon's default by
/// naming neither axis, so half a grid is refused at the flag layer,
/// before anything is dialed.
#[test]
fn spawn_with_only_one_geometry_flag_is_a_usage_error() {
    for argv in [
        ["sessions", "spawn", "--rows", "40"],
        ["sessions", "spawn", "--cols", "132"],
    ] {
        let out = cli_on("/tmp/felis-absent.sock")
            .args(argv)
            .output()
            .expect("run felis sessions spawn with half a geometry");
        assert_eq!(
            out.status.code(),
            Some(2),
            "{argv:?} is a usage error: stderr={}",
            String::from_utf8_lossy(&out.stderr),
        );
        assert!(
            out.stdout.is_empty(),
            "a refused spawn must print no session id",
        );
    }
}

/// Exit 1, not 2: the daemon answered "full", and the remedy (reap,
/// then retry) is a script's to take.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_past_the_session_cap_is_typed_at_capacity() {
    let tmp = private_dir();
    let (server, socket, pool) = spawn_daemon_with_caps(
        &tmp,
        quiet_factory(),
        DaemonCaps {
            max_sessions: 1,
            ..DaemonCaps::default()
        },
    );

    let first = felis_out(&socket, &["sessions", "spawn"]);
    assert!(
        first.status.success(),
        "the first spawn fills the cap: stderr={}",
        String::from_utf8_lossy(&first.stderr),
    );

    let refused = felis_out(&socket, &["sessions", "spawn", "--format", "json"]);
    assert_eq!(
        refused.status.code(),
        Some(1),
        "a full daemon is a refused-but-well-formed request: stderr={}",
        String::from_utf8_lossy(&refused.stderr),
    );
    assert!(
        refused.stdout.is_empty(),
        "a refused spawn prints no id object",
    );
    let object = schema::stderr_object(&refused);
    assert_eq!(object["error"]["kind"], "at_capacity");
    let message = object["error"]["message"].as_str().unwrap();
    assert!(message.contains("at 1 of 1 sessions"), "{message}");

    assert_eq!(
        pool.lock().await.ids().count(),
        1,
        "the refused spawn left the pool at the cap",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_tag_labels_the_session_at_creation() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory());

    let out = felis_out(
        &socket,
        &[
            "sessions", "spawn", "--tag", "agent", "--tag", "build", "--", "/bin/sh", "-c",
            "read x",
        ],
    );
    assert!(
        out.status.success(),
        "spawn --tag must succeed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let id_hex = String::from_utf8_lossy(&out.stdout).trim().to_string();

    let info = felis_out(&socket, &["sessions", "info", &id_hex, "--format", "json"]);
    let stdout = String::from_utf8_lossy(&info.stdout);
    assert!(
        stdout.contains("\"tags\":[\"agent\",\"build\"]"),
        "info --format json must carry the creation tags sorted; got {stdout}",
    );

    let hit = felis_out(&socket, &["sessions", "list", "--tag", "agent"]);
    assert!(
        String::from_utf8_lossy(&hit.stdout).contains(&id_hex[..8]),
        "list --tag agent must include the spawned session",
    );
    let miss = felis_out(&socket, &["sessions", "list", "--tag", "other"]);
    assert!(
        !String::from_utf8_lossy(&miss.stdout).contains(&id_hex[..8]),
        "list --tag other must not include the spawned session",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_json_emits_id_object() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory());

    let out = felis_out(
        &socket,
        &[
            "sessions", "spawn", "--format", "json", "--", "/bin/sh", "-c", "read x",
        ],
    );
    assert!(
        out.status.success(),
        "spawn --format json must succeed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let object: serde_json::Value =
        serde_json::from_str(&line).unwrap_or_else(|err| panic!("spawn json {line:?}: {err}"));
    assert_eq!(object["v"], 1, "every machine object carries the version");
    let hex = object["id"].as_str().expect("id string");
    assert_eq!(hex.len(), 32, "id must be 32-char hex; got `{hex}`");
    u128::from_str_radix(hex, 16).expect("hex parse");
    assert!(
        object.get("short_id").is_none(),
        "a rosterless reply must not claim a unique prefix: {line:?}",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tag_json_emits_id_and_tag_array_for_adds_and_removes() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory());
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let tag = felis_out(
        &socket,
        &[
            "sessions", "tag", "--format", "json", &id_hex, "work", "agent",
        ],
    );
    assert!(
        tag.status.success(),
        "tag --format json must succeed: status={:?} stderr={}",
        tag.status,
        String::from_utf8_lossy(&tag.stderr),
    );
    let tag_line = String::from_utf8_lossy(&tag.stdout).trim().to_string();
    let tagged = parse_point(&tag_line, "sessions tag");
    assert_eq!(tagged["id"], id_hex.as_str());
    assert_eq!(tagged["tags"], serde_json::json!(["agent", "work"]));

    let relabel = felis_out(
        &socket,
        &[
            "sessions", "tag", "--format", "json", &id_hex, "done", "--remove", "agent",
        ],
    );
    assert!(
        relabel.status.success(),
        "tag --format json add+remove must succeed: status={:?} stderr={}",
        relabel.status,
        String::from_utf8_lossy(&relabel.stderr),
    );
    let relabeled = parse_point(
        String::from_utf8_lossy(&relabel.stdout).trim(),
        "sessions tag add+remove",
    );
    assert_eq!(relabeled["id"], id_hex.as_str());
    assert_eq!(
        relabeled["tags"],
        serde_json::json!(["done", "work"]),
        "one call must apply the add and the remove together",
    );

    let untag = felis_out(
        &socket,
        &[
            "sessions", "tag", "--format", "json", &id_hex, "--remove", "work", "--remove", "done",
        ],
    );
    assert!(
        untag.status.success(),
        "tag --remove --format json must succeed: status={:?} stderr={}",
        untag.status,
        String::from_utf8_lossy(&untag.stderr),
    );
    let untagged = parse_point(
        String::from_utf8_lossy(&untag.stdout).trim(),
        "sessions tag --remove",
    );
    assert_eq!(untagged["id"], id_hex.as_str());
    assert_eq!(
        untagged["tags"],
        serde_json::json!([]),
        "emptied tag set must still emit a present `tags` array",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill_and_evict_json_emit_their_typed_replies() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory());
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let evict = felis_out(&socket, &["sessions", "evict", "--format", "json", &id_hex]);
    assert!(evict.status.success(), "evict --format json must exit 0");
    let evicted = parse_point(&String::from_utf8_lossy(&evict.stdout), "sessions evict");
    assert_eq!(evicted["id"], id_hex.as_str());
    assert_eq!(evicted["was_attached"], false);

    let kill = felis_out(&socket, &["sessions", "kill", "--format", "json", &id_hex]);
    assert!(kill.status.success(), "kill --format json must exit 0");
    let killed = parse_point(&String::from_utf8_lossy(&kill.stdout), "sessions kill");
    assert_eq!(killed["id"], id_hex.as_str());
    assert!(
        killed.get("was_attached").is_none(),
        "kill has no attachment verdict to report: {killed}",
    );

    server.abort();
}

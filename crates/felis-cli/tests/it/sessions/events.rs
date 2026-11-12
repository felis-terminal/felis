use super::*;

/// `sessions events` streams only what it names, so a kind added later
/// never reaches an existing invocation: omitting `--kind` is refused at
/// the flag layer, before anything is dialed.
#[test]
fn events_without_a_kind_is_a_usage_error() {
    let out = cli_on("/tmp/felis-absent.sock")
        .args(["sessions", "events", "--format", "jsonl"])
        .output()
        .expect("run felis sessions events without --kind");
    assert_eq!(
        out.status.code(),
        Some(2),
        "omitting --kind is a usage error: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.stdout.is_empty(), "a usage error writes no stream");
}

/// `sessions events` streams the kinds it was asked for as `type`-tagged
/// items: once a prompt from a session spawned earlier proves the
/// stream is open, a later spawn shows up as `added` and its kill as
/// `removed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn events_streams_lifecycle_and_prompt_items() {
    use tokio::io::{AsyncBufReadExt as _, BufReader};

    let tmp = private_dir();
    let factory = sh_factory("while :; do printf '\\033]133;A\\007'; sleep 0.2; done");
    let (server, socket, _pool) = spawn_daemon(&tmp, factory);
    let first = format!("{:032x}", create_and_detach(&socket).await);

    let mut child = tokio::process::Command::from(cli_on(&socket))
        .args([
            "sessions",
            "events",
            "--kind",
            "lifecycle,prompt",
            "--format",
            "jsonl",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("run felis sessions events");
    let mut lines = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    let mut next = async || -> serde_json::Value {
        let line = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
            .await
            .expect("an item within the timeout")
            .expect("read stdout")
            .expect("the stream is still open");
        let object: serde_json::Value =
            serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"));
        assert_eq!(object["v"], 1, "every object carries the epoch: {line}");
        object
    };
    loop {
        let item = next().await;
        if item["type"] == "prompt_shown" && item["session_id"] == first.as_str() {
            break;
        }
    }

    let spawned = felis_out(&socket, &["sessions", "spawn"]);
    assert!(spawned.status.success(), "{spawned:?}");
    let second = String::from_utf8_lossy(&spawned.stdout).trim().to_owned();
    let mine = |item: &serde_json::Value| item["session_id"] == second.as_str();
    let added = loop {
        let item = next().await;
        if mine(&item) {
            break item;
        }
    };
    assert_eq!(added["type"], "added", "{added}");

    let killed = felis_out(&socket, &["sessions", "kill", &second]);
    assert!(killed.status.success(), "{killed:?}");
    loop {
        let item = next().await;
        if mine(&item) && item["type"] == "removed" {
            break;
        }
        assert!(
            !(mine(&item) && item["type"] == "exited"),
            "a killed session reports no exit: {item}"
        );
    }

    drop(child.kill().await);
    server.abort();
}

/// `daemon stop` ends a `sessions events` stream with its clean
/// terminal, counting the items delivered, rather than leaving the
/// consumer a bare close it would read as a lost daemon.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn events_end_cleanly_when_the_daemon_stops() {
    use tokio::io::{AsyncBufReadExt as _, BufReader};

    let tmp = private_dir();
    let factory = sh_factory("while :; do printf '\\033]133;A\\007'; sleep 0.2; done");
    let (server, socket, _pool) = spawn_daemon(&tmp, factory);
    let id = format!("{:032x}", create_and_detach(&socket).await);
    let mut child = tokio::process::Command::from(cli_on(&socket))
        .args([
            "sessions",
            "events",
            "--kind",
            "lifecycle,prompt",
            "--format",
            "jsonl",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("run felis sessions events");
    let mut lines = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    let mut next = async || -> Option<serde_json::Value> {
        let line = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
            .await
            .expect("a line within the timeout")
            .expect("read stdout")?;
        Some(serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}")))
    };
    // A prompt proves the stream is open before the stop is sent.
    let mut items = 0;
    loop {
        let item = next().await.expect("an item");
        items += 1;
        if item["type"] == "prompt_shown" {
            break;
        }
    }

    let stopped = felis_out(&socket, &["daemon", "stop", "--force", "--format", "json"]);
    assert_eq!(stopped.status.code(), Some(0), "{stopped:?}");

    let mut removed = false;
    let terminal = loop {
        let item = next().await.expect("the stream ends in a terminal");
        if item.get("event").is_some() {
            break item;
        }
        items += 1;
        removed |= item["type"] == "removed" && item["session_id"] == id.as_str();
    };
    assert!(removed, "the destroyed session's removal precedes the end");
    assert_eq!(terminal["event"], "end", "{terminal}");
    assert_eq!(terminal["count"], items, "{terminal}");
    assert!(next().await.is_none(), "the terminal is the last line");
    let status = child.wait().await.expect("the verb exits");
    assert_eq!(status.code(), Some(0), "a clean end exits 0");
    server.abort();
}

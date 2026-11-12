//! `felis sessions` integration tests: a real daemon in-process, the
//! `felis` binary driven against it, exit codes per
//! docs/reference/ipc.md "CLI clients".

use std::process::Command as StdCommand;
use std::sync::Arc;
use std::time::Duration;

use felis_daemon::{
    SessionPool,
    serve::{DaemonCaps, SessionFactory, bind_unix},
};
use felis_protocol::messages::SpawnArgs;
use tempfile::TempDir;
use tokio::sync::Mutex;

use crate::common::{
    cli::{cli_bin, cli_command, cli_on, felis_command, felis_out},
    fixtures, schema,
};

use fixtures::{
    create_and_detach, fixture_path, poll_until, private_dir, quiet_factory, sh_factory,
    spawn_daemon, spawn_daemon_with_caps,
};

mod capture;
mod events;
mod failure_matrix;
mod globals_external;
mod notifications;
mod roster;
mod search;
mod send;
mod spawn_tag;
mod switch_retarget;
mod wait;

fn parse_jsonl(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|err| {
                panic!("every stdout line is one JSON object: {line} ({err})")
            })
        })
        .collect()
}

fn assert_all_versioned(objects: &[serde_json::Value], verb: &str) {
    assert!(!objects.is_empty(), "{verb} emitted nothing");
    for object in objects {
        assert_eq!(
            object["v"], 1,
            "{verb} emitted an object with no v:1: {object}"
        );
        schema::assert_cli_object(object);
    }
}

fn parse_point(stdout: &str, verb: &str) -> serde_json::Value {
    let object: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|err| panic!("{verb} must emit one JSON object: {stdout} ({err})"));
    assert_eq!(object["v"], 1, "{verb} emitted an object with no v:1");
    schema::assert_cli_object(&object);
    object
}

/// Bound before the fake daemon is spawned, so the CLI's connect is
/// queued rather than refused.
fn bind_listener(socket: &std::path::Path) -> felis_transport::Listener {
    felis_transport::Listener::bind(&felis_transport::Endpoint::unix(socket.to_path_buf()))
        .expect("bind the fake daemon's socket")
}

fn assert_framed_failure(out: &std::process::Output, kind: &str, code: i32) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(code), "stderr={stderr}");
    assert!(
        out.stdout.is_empty(),
        "a failure writes no result: {}",
        String::from_utf8_lossy(&out.stdout),
    );
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "stderr is one object: {stderr}");
    let object = parse_point(lines[0], "the failure object");
    assert_eq!(object["error"]["kind"], kind, "{stderr}");
    assert!(object["error"]["message"].is_string(), "{stderr}");
}

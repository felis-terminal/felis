//! `felis fonts list` integration tests (docs/reference/cli.md "Fonts"):
//! the `felis` binary is copied beside a stand-in `felis-client`, which
//! it prefers over any on `PATH`, so each test picks the frontend's
//! answer.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

#[path = "common/schema.rs"]
mod schema;

const LISTING: &str = r#"{"v":2,"families":[{"name":"DejaVu Sans Mono","monospace":true,"styles":[{"weight":400,"italic":false},{"weight":700,"italic":true}]},{"name":"Noto Color Emoji","monospace":false,"styles":[{"weight":400,"italic":false}]}]}"#;

/// A `felis` whose sibling frontend runs `frontend` as a `sh` body, or
/// none at all.
fn install(frontend: Option<&str>) -> (TempDir, PathBuf) {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = TempDir::new().unwrap();
    let felis = dir.path().join("felis");
    std::fs::copy(env!("CARGO_BIN_EXE_felis"), &felis).unwrap();
    if let Some(script) = frontend {
        let client = dir.path().join("felis-client");
        std::fs::write(&client, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&client, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (dir, felis)
}

fn fonts_list(felis: &Path, extra: &[&str]) -> Output {
    Command::new(felis)
        .args(["fonts", "list"])
        .args(extra)
        .env("PATH", "")
        .env_remove("FELIS_SOCKET")
        .output()
        .unwrap()
}

fn error_object(out: &Output) -> serde_json::Value {
    let stderr = String::from_utf8(out.stderr.clone()).unwrap();
    let line = stderr.lines().find(|l| l.starts_with('{')).unwrap();
    let object: serde_json::Value = serde_json::from_str(line).unwrap();
    schema::assert_cli_object(&object);
    object
}

#[test]
fn human_output_is_one_family_per_line_under_a_header() {
    let (_dir, felis) = install(Some(&format!("echo '{LISTING}'")));
    let out = fonts_list(&felis, &[]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "FAMILY            MONO  STYLES\n\
         DejaVu Sans Mono  yes   Regular, Bold Italic\n\
         Noto Color Emoji  no    Regular\n"
    );
}

#[test]
fn json_output_is_one_point_object() {
    let (_dir, felis) = install(Some(&format!("echo '{LISTING}'")));
    let out = fonts_list(&felis, &["--format", "json"]);
    assert!(out.status.success(), "{out:?}");
    let object: serde_json::Value =
        serde_json::from_str(String::from_utf8(out.stdout).unwrap().trim()).unwrap();
    schema::assert_cli_object(&object);
    assert_eq!(
        object,
        serde_json::json!({"v": 1, "families": [
            {"name": "DejaVu Sans Mono", "monospace": true, "styles": [
                {"weight": 400, "italic": false},
                {"weight": 700, "italic": true},
            ]},
            {"name": "Noto Color Emoji", "monospace": false, "styles": [
                {"weight": 400, "italic": false},
            ]},
        ]})
    );
}

#[test]
fn a_frontend_that_predates_the_verb_exits_two_as_unsupported() {
    let (_dir, felis) = install(Some(
        r#"echo "error: unexpected argument '$1' found" >&2; exit 2"#,
    ));
    let out = fonts_list(&felis, &["--format", "json"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "a failure writes no result object");
    let object = error_object(&out);
    assert_eq!(object["error"]["kind"], "unsupported");
}

#[test]
fn a_headless_install_exits_two_as_unsupported() {
    let (_dir, felis) = install(None);
    let out = fonts_list(&felis, &["--format", "json"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(error_object(&out)["error"]["kind"], "unsupported");
}

#[test]
fn the_global_carrier_and_config_flags_are_refused() {
    let (_dir, felis) = install(Some(&format!("echo '{LISTING}'")));
    for globals in [
        &["--host", "vm"][..],
        &["--socket", "/nonexistent/felis.sock"],
        &["--config", "/nonexistent/config.toml"],
    ] {
        let out = Command::new(&felis)
            .args(globals)
            .args(["fonts", "list"])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{globals:?}");
        assert!(out.stdout.is_empty(), "{globals:?}");
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(stderr.contains("felis fonts"), "{globals:?}: {stderr}");
    }
}

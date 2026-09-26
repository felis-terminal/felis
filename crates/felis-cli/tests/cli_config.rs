//! `felis config path|check|show-effective` integration tests
//! (docs/reference/cli.md "Config verbs"). The suite selects its
//! document with `--config`; discovery, which no environment variable
//! can redirect on Windows, is tested per platform: a temp profile on
//! Unix, the frozen path spelling (docs/reference/config.md) on Windows.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command as StdCommand;

use tempfile::TempDir;

#[path = "common/schema.rs"]
mod schema;

fn config_path(home: &TempDir) -> std::path::PathBuf {
    home.path().join("config.toml")
}

fn cli(home: &TempDir) -> StdCommand {
    let mut cmd = bare_cli();
    cmd.arg("--config").arg(config_path(home));
    cmd
}

fn bare_cli() -> StdCommand {
    let bin = std::env::var("CARGO_BIN_EXE_felis").expect("cargo sets CARGO_BIN_EXE_felis");
    let mut cmd = StdCommand::new(bin);
    cmd.env_remove("FELIS_SOCKET");
    cmd
}

fn write_config(home: &TempDir, text: &str) -> std::path::PathBuf {
    let path = config_path(home);
    std::fs::write(&path, text).unwrap();
    path
}

fn json_of(out: &std::process::Output) -> serde_json::Value {
    let stdout = String::from_utf8(out.stdout.clone()).unwrap();
    let object: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("not one JSON object: {e}\n{stdout}"));
    schema::assert_cli_object(&object);
    object
}

/// The spelling of `path` a child launched there will report back:
/// `canonicalize` resolves the `/private/tmp` macOS reports as its
/// working directory, and on Windows its `\\?\C:\…` verbatim form is
/// reduced to the plain one, which is the only form `current_dir`
/// echoes.
#[cfg(windows)]
fn invoking_dir(path: &std::path::Path) -> std::path::PathBuf {
    let real = std::fs::canonicalize(path).unwrap();
    let text = real.display().to_string();
    text.strip_prefix(r"\\?\")
        .map_or(real, std::path::PathBuf::from)
}

#[cfg(not(windows))]
fn invoking_dir(path: &std::path::Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap()
}

#[test]
fn path_reports_the_location_before_the_file_exists() {
    let home = TempDir::new().unwrap();
    let out = cli(&home)
        .args(["config", "path", "--format", "json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let object = json_of(&out);
    assert_eq!(object["v"], 1);
    assert_eq!(object["exists"], false);
    let path = object["path"].as_str().unwrap();
    assert!(path.ends_with("config.toml"), "{path}");

    write_config(&home, "");
    let out = cli(&home)
        .args(["config", "path", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(json_of(&out)["exists"], true);
}

/// Without the flag the platform default is still discovered, and its
/// absent file is still the first-run case rather than an error.
///
/// Unix only: on Windows the known-folder API reads no environment
/// variable, so the child cannot be pointed at a temp profile.
#[cfg(unix)]
#[test]
fn default_discovery_is_untouched_when_the_flag_is_absent() {
    let home = TempDir::new().unwrap();
    let mut cmd = bare_cli();
    cmd.env("HOME", home.path());
    cmd.env("XDG_CONFIG_HOME", home.path().join("config"));
    let out = cmd
        .args(["config", "check", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let object = json_of(&out);
    assert_eq!(object["exists"], false);
    assert_eq!(object["errors"], 0);
    let path = object["path"].as_str().unwrap();
    assert!(
        path.starts_with(home.path().to_str().unwrap()),
        "discovery must still resolve under the temp home: {path}"
    );
}

/// The Windows row of the frozen platform table
/// (`docs/reference/config.md`), read back off the binary: the doubled
/// `config\` segment is the documented path, not a bug to flatten. It
/// asserts a spelling and never a file, because the known-folder API
/// points at the real profile this suite must not touch.
#[cfg(windows)]
#[test]
fn default_discovery_reports_the_roaming_app_data_path() {
    let out = bare_cli()
        .args(["config", "path", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let path = json_of(&out)["path"].as_str().unwrap().to_owned();
    assert!(
        path.ends_with(r"felis\config\config.toml"),
        "the `config\\` segment is part of the frozen row: {path}"
    );
    let roaming = std::env::var("APPDATA").expect("Windows sets APPDATA");
    assert!(
        path.to_lowercase().starts_with(&roaming.to_lowercase()),
        "discovery must resolve under the roaming app-data folder: {path} (APPDATA={roaming})"
    );
}

/// A path the user typed that is not there is a typo, not a first run,
/// and every front door has to say so the same way.
#[test]
fn a_missing_selected_file_is_an_error_in_every_verb() {
    let home = TempDir::new().unwrap();
    let check = cli(&home)
        .args(["config", "check", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(check.status.code(), Some(1));
    let object = json_of(&check);
    assert_eq!(object["errors"], 1);
    assert_eq!(object["diagnostics"][0]["severity"], "error");
    assert_eq!(object["diagnostics"][0]["kind"], "missing_file");

    let effective = cli(&home)
        .args(["config", "show-effective", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(effective.status.code(), Some(1));

    // `path` still answers "where would it go", as it does for a
    // first-time user of the default location.
    let path = cli(&home)
        .args(["config", "path", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(path.status.code(), Some(0));
    assert_eq!(json_of(&path)["exists"], false);
}

/// A read failure is its own diagnostic kind, distinct from "not there"
/// and from "does not parse".
///
/// A directory at the selected path is the portable way to fail the
/// read: mode bits are Unix-only and root defeats them anyway.
#[test]
fn an_unreadable_selected_file_reports_io() {
    let home = TempDir::new().unwrap();
    std::fs::create_dir(config_path(&home)).unwrap();
    let out = cli(&home)
        .args(["config", "check", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let object = json_of(&out);
    assert_eq!(object["errors"], 1);
    assert_eq!(object["diagnostics"][0]["kind"], "io");
}

/// The flag names one document; disagreeing about which one would make
/// `check` a report on a file `show-effective` never read.
#[test]
fn every_verb_reports_the_same_selected_path() {
    let home = TempDir::new().unwrap();
    let written = write_config(&home, "[font]\nsize_px = 12.0\n");
    let mut reported = Vec::new();
    for verb in ["path", "check", "show-effective"] {
        let out = cli(&home)
            .args(["config", verb, "--format", "json"])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{verb}");
        reported.push(json_of(&out)["path"].as_str().unwrap().to_owned());
    }
    assert_eq!(reported[0], written.display().to_string());
    assert!(reported.iter().all(|p| *p == reported[0]), "{reported:?}");
}

/// Relative to where the user typed it, not to wherever felis or the
/// window it launches happens to run.
#[test]
fn a_relative_selection_resolves_against_the_invoking_directory() {
    let home = TempDir::new().unwrap();
    let nested = home.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    let nested = invoking_dir(&nested);
    std::fs::write(nested.join("felis.toml"), "[font]\nsize_px = 9.0\n").unwrap();

    let out = bare_cli()
        .current_dir(&nested)
        .args([
            "--config",
            "felis.toml",
            "config",
            "path",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let object = json_of(&out);
    assert_eq!(object["exists"], true);
    assert_eq!(
        object["path"].as_str().unwrap(),
        nested.join("felis.toml").display().to_string(),
        "the reported path is always the absolute form"
    );
}

#[test]
fn check_reports_warnings_and_still_exits_zero() {
    let home = TempDir::new().unwrap();
    write_config(&home, "[font]\nfamily = \"JetBrains Mono\"\nsizee = 14.0\n");
    let out = cli(&home)
        .args(["config", "check", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let object = json_of(&out);
    assert_eq!(object["v"], 1);
    assert_eq!(object["errors"], 0);
    assert_eq!(object["warnings"], 1);
    let d = &object["diagnostics"][0];
    assert_eq!(d["severity"], "warning");
    assert_eq!(d["kind"], "unknown_key");
    assert_eq!(d["key"], "font.sizee");
}

#[test]
fn check_warns_on_an_out_of_range_opacity_and_scroll_multiplier() {
    let home = TempDir::new().unwrap();
    write_config(
        &home,
        "[window]\nopacity = 2\n\n[mouse]\nscroll_multiplier = -1\n",
    );
    let out = cli(&home)
        .args(["config", "check", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let object = json_of(&out);
    assert_eq!(object["errors"], 0);
    assert_eq!(object["warnings"], 2);
    let reported: Vec<(&str, &str)> = object["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            assert_eq!(d["severity"], "warning");
            assert_eq!(d["kind"], "value");
            (d["key"].as_str().unwrap(), d["message"].as_str().unwrap())
        })
        .collect();
    assert!(
        reported.contains(&("window.opacity", "2 is outside [0.0, 1.0]; clamped to 1")),
        "{reported:?}"
    );
    assert!(
        reported.contains(&(
            "mouse.scroll_multiplier",
            "-1 is outside [0.1, 100.0]; clamped to 0.1"
        )),
        "{reported:?}"
    );
}

#[test]
fn check_warns_on_an_unknown_enum_token_and_still_exits_zero() {
    let home = TempDir::new().unwrap();
    write_config(
        &home,
        concat!(
            "[font]\nfamily = \"JetBrains Mono\"\n",
            "[cursor]\nblink = \"breathe\"\n",
            "[client.felis.shader]\npost = { builtin = \"rain\" }\n",
        ),
    );
    let out = cli(&home)
        .args(["config", "check", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let object = json_of(&out);
    assert_eq!(object["errors"], 0);
    assert_eq!(object["warnings"], 2);
    let keys: Vec<&str> = object["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            assert_eq!(d["kind"], "value");
            d["key"].as_str().unwrap()
        })
        .collect();
    assert!(keys.contains(&"cursor.blink"), "{keys:?}");
    assert!(
        keys.contains(&"client.felis.shader.post.builtin"),
        "{keys:?}"
    );

    let shown = cli(&home)
        .args(["config", "show-effective", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(shown.status.code(), Some(0));
    let object = json_of(&shown);
    assert_eq!(
        object["config"]["font"]["family"], "JetBrains Mono",
        "an unknown token must not reset the rest of the document: {object}"
    );
}

#[test]
fn a_font_size_written_as_an_integer_checks_clean_and_shows_the_float_value() {
    let home = TempDir::new().unwrap();
    write_config(&home, "[font]\nsize_px = 14\n");
    let checked = cli(&home)
        .args(["config", "check", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(checked.status.code(), Some(0));
    let object = json_of(&checked);
    assert_eq!(object["errors"], 0);
    assert_eq!(object["warnings"], 0);

    let shown = cli(&home)
        .args(["config", "show-effective", "--format", "json"])
        .output()
        .unwrap();
    let integer = json_of(&shown);

    write_config(&home, "[font]\nsize_px = 14.0\n");
    let shown = cli(&home)
        .args(["config", "show-effective", "--format", "json"])
        .output()
        .unwrap();
    let float = json_of(&shown);

    assert_eq!(integer["config"]["font"]["size_px"], 14.0);
    assert_eq!(integer, float);
}

#[test]
fn check_on_a_parse_failure_exits_one_with_the_error_reported() {
    let home = TempDir::new().unwrap();
    write_config(&home, "[font\nfamily = \"X\"\n");
    let out = cli(&home)
        .args(["config", "check", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let object = json_of(&out);
    assert_eq!(object["errors"], 1);
    assert_eq!(object["diagnostics"][0]["severity"], "error");
    assert_eq!(object["diagnostics"][0]["kind"], "parse");
}

#[test]
fn check_reports_the_complete_set_not_the_first_problem() {
    let home = TempDir::new().unwrap();
    write_config(
        &home,
        concat!(
            "[font]\n",
            "nope = 1\n",
            "[window]\n",
            "alsonope = true\n",
            "[shader]\n",
            "post = { file = \"/definitely/not/here.wgsl\" }\n",
        ),
    );
    let out = cli(&home)
        .args(["config", "check", "--format", "json"])
        .output()
        .unwrap();
    let object = json_of(&out);
    let kinds: Vec<&str> = object["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["kind"].as_str().unwrap())
        .collect();
    assert!(
        kinds.iter().filter(|k| **k == "unknown_key").count() >= 2,
        "{kinds:?}"
    );
    // A missing file is a warning, as the live client's
    // fall-back-and-warn treats it.
    assert!(kinds.contains(&"missing_file"), "{kinds:?}");
    assert_eq!(out.status.code(), Some(0), "warnings alone stay exit 0");
}

#[test]
fn other_clients_overlays_are_never_validated() {
    let home = TempDir::new().unwrap();
    write_config(
        &home,
        concat!(
            "[font]\n",
            "size_px = 12.0\n",
            "[client.other]\n",
            "not_a_felis_key = \"anything\"\n",
            "[client.other.whatever]\n",
            "deep = 3\n",
        ),
    );
    let out = cli(&home)
        .args(["config", "check", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let object = json_of(&out);
    assert_eq!(
        object["diagnostics"].as_array().unwrap().len(),
        0,
        "another client's section must produce no diagnostic: {object}"
    );
}

#[test]
fn show_effective_resolves_the_named_clients_overlay() {
    let home = TempDir::new().unwrap();
    write_config(
        &home,
        concat!(
            "[font]\n",
            "size_px = 12.0\n",
            "family = \"Base Mono\"\n",
            "[client.tui.font]\n",
            "size_px = 20.0\n",
            "[client.other]\n",
            "untouched = true\n",
        ),
    );

    let base = cli(&home)
        .args(["config", "show-effective", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(base.status.code(), Some(0));
    let object = json_of(&base);
    assert_eq!(object["v"], 1);
    assert_eq!(object["client"], "felis");
    assert_eq!(object["config"]["font"]["size_px"], 12.0);

    let overlaid = cli(&home)
        .args([
            "config",
            "show-effective",
            "--client",
            "tui",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    let object = json_of(&overlaid);
    assert_eq!(object["client"], "tui");
    assert_eq!(object["config"]["font"]["size_px"], 20.0);
    assert_eq!(object["config"]["font"]["family"], "Base Mono");
    assert_eq!(object["config"]["client"]["other"]["untouched"], true);
}

#[test]
fn show_effective_human_renders_toml() {
    let home = TempDir::new().unwrap();
    write_config(&home, "[font]\nfamily = \"Base Mono\"\n");
    let out = cli(&home)
        .args(["config", "show-effective"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("family = \"Base Mono\""), "{stdout}");
    let _reparsed: toml::Table = stdout.parse().expect("the rendered config must be TOML");
}

/// Past a parse error the loader hands back the defaults; printing
/// those would claim the user's file was applied.
#[test]
fn show_effective_refuses_a_document_with_errors() {
    let home = TempDir::new().unwrap();
    write_config(&home, "[font\n");
    let out = cli(&home)
        .args(["config", "show-effective", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "a failure writes no result object");
    let stderr = String::from_utf8(out.stderr).unwrap();
    let line = stderr.lines().find(|l| l.starts_with('{')).unwrap();
    let object: serde_json::Value = serde_json::from_str(line).unwrap();
    schema::assert_cli_object(&object);
    assert_eq!(object["v"], 1);
    assert!(
        object["error"]["message"]
            .as_str()
            .unwrap()
            .contains("felis config check"),
        "{object}"
    );
}

#[test]
fn config_verbs_never_report_a_daemon_failure() {
    let home = TempDir::new().unwrap();
    write_config(&home, "[font]\nsize_px = 12.0\n");
    for args in [
        vec!["config", "path"],
        vec!["config", "check"],
        vec!["config", "show-effective"],
    ] {
        let out = cli(&home).args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{args:?}");
    }
}

/// Honoring a carrier silently would check this machine's file under a
/// flag that promised the remote's.
#[test]
fn a_carrier_flag_on_a_config_verb_is_a_usage_error() {
    let home = TempDir::new().unwrap();
    for carrier in [
        vec!["--host".to_owned(), "example.invalid".to_owned()],
        vec![
            "--socket".to_owned(),
            home.path().join("nothing.sock").display().to_string(),
        ],
    ] {
        for verb in ["path", "check", "show-effective"] {
            let out = cli(&home)
                .args(&carrier)
                .args(["config", verb])
                .output()
                .unwrap();
            assert_eq!(
                out.status.code(),
                Some(2),
                "{carrier:?} on config {verb}: stderr={}",
                String::from_utf8_lossy(&out.stderr),
            );
            let stderr = String::from_utf8(out.stderr).unwrap();
            assert!(stderr.contains("felis config:"), "{stderr}");
            // Argument parsing precedes format selection, so no machine
            // object.
            assert!(out.stdout.is_empty(), "{stderr}");
        }
    }
}

/// A stream verb's refusal is that stream's one terminal on stdout. A
/// point-framed one would leave the consumer with an EOF and no
/// terminal, which the contract calls a protocol failure
/// (docs/reference/cli.md "The envelope").
#[test]
fn a_stream_verb_refuses_the_flag_in_its_own_framing() {
    let home = TempDir::new().unwrap();
    let out = bare_cli()
        .arg("--config")
        .arg(config_path(&home))
        .args(["notifications", "subscribe", "--format", "jsonl"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let object = json_of(&out);
    assert_eq!(object["v"], 1);
    assert_eq!(object["event"], "error");
    assert_eq!(object["error"]["kind"], "usage");
    assert!(
        out.stderr.is_empty(),
        "a machine framing writes nothing else: {}",
        String::from_utf8_lossy(&out.stderr),
    );
}

/// The launch path is the flag's headline use and the one reader with
/// no report of its own: the frontend would fall back to the built-in
/// defaults, so a typo has to be caught before the exec.
#[test]
fn a_window_launch_refuses_a_selection_that_is_not_there() {
    let home = TempDir::new().unwrap();
    let out = bare_cli()
        .arg("--config")
        .arg(config_path(&home))
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "stdout={}",
        String::from_utf8_lossy(&out.stdout),
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("no config file at"), "{stderr}");
}

use super::*;

#[test]
fn an_external_commands_own_trace_perf_flag_forwards_opaquely() {
    let external = external_command(&argv(&["felis", "tui", "--trace-perf"]))
        .expect("an external command's own --trace-perf forwards opaquely");
    assert_eq!(external.args, argv(&["--trace-perf"]));
}

#[test]
fn an_external_command_gets_its_arguments_verbatim() {
    assert_eq!(
        external_command(&argv(&["felis", "tui", "attach", "1a2b", "--fullscreen"])),
        Some(External {
            word: "tui".into(),
            args: argv(&["attach", "1a2b", "--fullscreen"]),
            globals_before: vec![],
        }),
    );
}

#[test]
fn a_built_in_verb_or_a_flag_line_is_never_an_external_command() {
    for line in [
        &["felis"][..],
        &["felis", "sessions", "list"],
        &["felis", "help"],
        &["felis", "help", "sessions"],
        &["felis", "help", "help"],
        &["felis", "--", "htop"],
        &["felis", "--config", "c.toml", "--", "htop"],
        &["felis", "--help", "tui"],
        &["felis", "-V", "tui"],
        &["felis", "--fullscreen", "tui"],
        &["felis", "--config", "--help", "tui"],
        &["felis", "--host", "h", "--ssh-arg", "-p", "tui"],
    ] {
        assert_eq!(external_command(&argv(line)), None, "{line:?}");
    }
}

#[test]
fn help_for_an_external_command_forwards_to_its_own_help() {
    let external = external_command(&argv(&["felis", "help", "tui", "attach"]))
        .expect("`help <external>` is an external command");
    assert_eq!(external.word, "tui");
    assert_eq!(external.args, argv(&["attach", "--help"]));
}

#[test]
fn globals_before_an_external_command_are_recorded_for_the_refusal() {
    let external = external_command(&argv(&[
        "felis",
        "--config=c.toml",
        "--host",
        "h",
        "--ssh-arg=-p",
        "--class",
        "scratch",
        "tui",
    ]))
    .expect("globals before the word still reach the lookup");
    assert_eq!(external.word, "tui");
    assert_eq!(
        external.globals_before,
        ["--config", "--host", "--ssh-arg", "--class"]
    );
}

/// A separator would turn the sibling / `$PATH` lookup of `felis-<name>`
/// into "run this file".
#[test]
fn an_external_name_with_a_path_or_a_dash_is_not_looked_up() {
    for name in ["../evil", "/usr/bin/evil", "a/b", "./x", "-x", "", "a b"] {
        assert_eq!(external_name(name.as_ref()), None, "{name:?}");
    }
    assert_eq!(external_name("tui".as_ref()), Some("tui"));
}

#[test]
fn a_misspelled_verb_is_offered_the_built_in_it_resembles() {
    assert_eq!(similar_verb("session").as_deref(), Some("sessions"));
    assert_eq!(similar_verb("zzz"), None);
}

#[cfg(unix)]
#[test]
fn the_lookup_lists_executable_candidates_in_directory_order() {
    use std::os::unix::fs::PermissionsExt;
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let place = |dir: &tempfile::TempDir, mode: u32| {
        let path = dir.path().join("felis-tui");
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    };
    place(&dirs[0], 0o644);
    let first = place(&dirs[1], 0o755);
    let second = place(&dirs[2], 0o755);
    let order = dirs.iter().map(|d| d.path().to_path_buf());
    assert_eq!(
        find_external_in("felis-tui", order.clone()),
        [first, second],
        "a non-executable file is skipped, and directory order is kept",
    );
    assert_eq!(
        find_external_in("felis-absent", order),
        Vec::<PathBuf>::new()
    );
}

#[test]
fn wrap_exec_err_explains_a_missing_default_frontend_as_a_headless_build() {
    let err = wrap_exec_err(
        DEFAULT_FRONTEND_BIN,
        std::io::Error::from(std::io::ErrorKind::NotFound),
    );
    let msg = format!("{err}");
    assert!(msg.contains("headless build"), "got: {msg}");
    assert!(msg.contains(DEFAULT_FRONTEND_BIN), "got: {msg}");
}

#[test]
fn wrap_exec_err_wraps_other_io_errors_with_exec_context() {
    let err = wrap_exec_err(
        DEFAULT_FRONTEND_BIN,
        std::io::Error::from(std::io::ErrorKind::PermissionDenied),
    );
    let msg = format!("{err:#}");
    assert!(
        msg.contains(&format!("exec {DEFAULT_FRONTEND_BIN}")),
        "got: {msg}"
    );
    assert!(!msg.contains("headless build"), "got: {msg}");
}

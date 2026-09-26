use clap::Parser;
use felis_client_core::local_socket::resolve_local_socket;

use super::*;

/// An explicit `--socket` outranks every other source; a relative path
/// is anchored to the working directory the flag was typed in.
#[test]
fn resolve_local_socket_returns_override_when_set() {
    let abs = PathBuf::from("/tmp/felis-work.sock");
    assert_eq!(resolve_local_socket(Some(abs.as_path())).unwrap(), abs,);
    let rel = PathBuf::from("./felis.sock");
    #[cfg(unix)]
    assert_eq!(
        resolve_local_socket(Some(rel.as_path())).unwrap(),
        std::env::current_dir().unwrap().join(&rel),
    );
    #[cfg(windows)]
    assert_eq!(resolve_local_socket(Some(rel.as_path())).unwrap(), rel);
}

fn session(rows: u16, cols: u16) -> SessionInfo {
    SessionInfo {
        id: 1,
        dims: felis_protocol::messages::GridDims {
            rows,
            cols,
            pixel_w: 0,
            pixel_h: 0,
        },
        title: None,
        cwd: None,
        idle_seconds: None,
        tags: Vec::new(),
        last_notification: None,
        foreground: None,
        exited: false,
        last_exit_code: None,
        attachments: Vec::new(),
        sequence: std::num::NonZeroU64::MIN,
    }
}

#[test]
fn format_session_line_drops_empty_optional_labels() {
    let s = session(24, 80);
    assert_eq!(format_session_line(&s, "00000000"), "00000000  24x80",);
}

#[test]
fn format_session_line_marks_a_post_exit_grace_corpse() {
    let s = SessionInfo {
        exited: true,
        ..session(24, 80)
    };
    assert_eq!(
        format_session_line(&s, "00000000"),
        "00000000  24x80  (exited)",
    );
}

#[test]
fn format_session_line_includes_age_title_and_cwd_when_present() {
    let s = SessionInfo {
        title: Some("zsh: ~/work".into()),
        cwd: Some("file://localhost/home/me/work".into()),
        idle_seconds: Some(75),
        ..session(30, 100)
    };
    assert_eq!(
        format_session_line(&s, "00000000"),
        "00000000  30x100  1m  zsh: ~/work  file://localhost/home/me/work",
    );
}

#[test]
fn format_session_line_appends_each_tag_hash_prefixed() {
    let s = SessionInfo {
        tags: vec!["agent".into(), "work".into()],
        ..session(24, 80)
    };
    assert_eq!(
        format_session_line(&s, "00000000"),
        "00000000  24x80  #agent  #work",
    );
}

#[test]
fn format_session_line_shows_foreground_program_bracketed() {
    let s = SessionInfo {
        title: Some("a title".into()),
        idle_seconds: Some(3),
        foreground: Some("claude".into()),
        ..session(24, 80)
    };
    assert_eq!(
        format_session_line(&s, "00000000"),
        "00000000  24x80  3s  [claude]  a title",
    );
}

#[test]
fn format_session_line_appends_notification_status() {
    // Body wins over title.
    let s = SessionInfo {
        last_notification: Some(felis_protocol::messages::SessionNotification {
            notification: felis_protocol::messages::Notification {
                title: Some("claude".into()),
                body: "task complete".into(),
                urgency: felis_protocol::messages::Urgency::Normal,
            },
            age_seconds: 5,
        }),
        ..session(24, 80)
    };
    assert_eq!(
        format_session_line(&s, "00000000"),
        "00000000  24x80  !task complete (5s)",
    );
}

#[test]
fn notification_snippet_collapses_newlines_and_truncates() {
    assert_eq!(notification_snippet("line1\nline2"), "line1 line2");
    let long = "x".repeat(50);
    let snip = notification_snippet(&long);
    assert_eq!(snip.chars().count(), 41); // 40 chars + ellipsis
    assert!(snip.ends_with('…'));
}

#[test]
fn format_session_line_treats_empty_optional_strings_as_absent() {
    let s = SessionInfo {
        title: Some(String::new()),
        cwd: Some("   ".into()),
        ..session(1, 1)
    };
    assert_eq!(format_session_line(&s, "00000000"), "00000000  1x1",);
}

#[test]
fn format_idle_age_picks_one_unit_per_magnitude() {
    assert_eq!(format_idle_age(0), "0s");
    assert_eq!(format_idle_age(59), "59s");
    assert_eq!(format_idle_age(60), "1m");
    assert_eq!(format_idle_age(60 * 60 - 1), "59m");
    assert_eq!(format_idle_age(60 * 60), "1h");
    assert_eq!(format_idle_age(60 * 60 * 24 - 1), "23h");
    assert_eq!(format_idle_age(60 * 60 * 24), "1d");
    assert_eq!(format_idle_age(60 * 60 * 24 * 7), "7d");
}

/// Tokens after the program survive as separate argv entries, not re-split.
#[test]
fn cli_captures_trailing_command_after_double_dash() {
    let cli = Cli::try_parse_from(["felis", "--", "htop"]).expect("parses felis -- htop");
    assert!(cli.cmd.is_none());
    assert_eq!(cli.command, vec!["htop"]);

    let cli = Cli::try_parse_from(["felis", "--", "bash", "-c", "ls; exec bash"])
        .expect("parses felis -- bash -c '...'");
    assert_eq!(cli.command, vec!["bash", "-c", "ls; exec bash"]);

    let bare = Cli::try_parse_from(["felis"]).expect("parses bare felis");
    assert_eq!(bare.command, Vec::<String>::new());
}

/// Bare `-` is the sole stdin trigger.
#[test]
fn sessions_send_rejects_from_stdin_flag() {
    let err = Cli::try_parse_from(["felis", "sessions", "send", "--from-stdin", "1a", "x"]);
    assert!(
        err.is_err(),
        "--from-stdin is not accepted on `sessions send`",
    );
    Cli::try_parse_from(["felis", "sessions", "send", "1a", "-"])
        .expect("bare `-` stdin trigger parses");
}

/// The two stop postures are opposites, so the pair is refused before
/// any daemon is dialed.
#[test]
fn daemon_stop_refuses_force_and_when_empty_together() {
    for argv in [
        &["felis", "daemon", "stop"][..],
        &["felis", "daemon", "stop", "--force"],
        &["felis", "daemon", "stop", "--when-empty"],
    ] {
        Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?} must parse: {e}"));
    }
    assert!(
        Cli::try_parse_from(["felis", "daemon", "stop", "--force", "--when-empty"]).is_err(),
        "a stop cannot name two postures at once"
    );
}

#[test]
fn sessions_spawn_rejects_malformed_env_at_the_clap_layer() {
    let err = Cli::try_parse_from(["felis", "sessions", "spawn", "--env", "NOEQUALS"]);
    assert!(err.is_err(), "--env without `=` must fail to parse");
    Cli::try_parse_from(["felis", "sessions", "spawn", "--env", "KEY=VAL"])
        .expect("well-formed KEY=VAL still parses");
}

/// The point/stream classification is normative (docs/reference/cli.md
/// "Machine output"), so it is pinned per verb rather than sampled.
#[test]
fn each_verb_accepts_only_its_own_class_format() {
    // Point verbs.
    for argv in [
        &["felis", "sessions", "spawn"][..],
        &["felis", "sessions", "tag", "1a", "work"],
        &["felis", "sessions", "tag", "1a", "--remove", "work"],
        &["felis", "sessions", "kill", "1a"],
        &["felis", "sessions", "evict", "1a"],
        &["felis", "sessions", "send", "1a", "x"],
        &["felis", "sessions", "info", "1a"],
        &["felis", "sessions", "switch", "1a", "--from", "2b"],
        &["felis", "window", "retarget"],
        &["felis", "sessions", "list"],
    ] {
        let human: Vec<&str> = argv.iter().copied().chain(["--format", "human"]).collect();
        Cli::try_parse_from(&human).unwrap_or_else(|e| panic!("{human:?} must parse: {e}"));
        let point: Vec<&str> = argv.iter().copied().chain(["--format", "json"]).collect();
        Cli::try_parse_from(&point).unwrap_or_else(|e| panic!("{point:?} must parse: {e}"));
        let wrong: Vec<&str> = argv.iter().copied().chain(["--format", "jsonl"]).collect();
        assert!(
            Cli::try_parse_from(&wrong).is_err(),
            "{wrong:?} must refuse the stream framing"
        );
    }

    // Stream verbs.
    for argv in [
        &["felis", "sessions", "capture", "1a"][..],
        &["felis", "sessions", "search", "1a", "pat"],
        &["felis", "notifications", "subscribe"],
    ] {
        let human: Vec<&str> = argv.iter().copied().chain(["--format", "human"]).collect();
        Cli::try_parse_from(&human).unwrap_or_else(|e| panic!("{human:?} must parse: {e}"));
        let stream: Vec<&str> = argv.iter().copied().chain(["--format", "jsonl"]).collect();
        Cli::try_parse_from(&stream).unwrap_or_else(|e| panic!("{stream:?} must parse: {e}"));
        let wrong: Vec<&str> = argv.iter().copied().chain(["--format", "json"]).collect();
        assert!(
            Cli::try_parse_from(&wrong).is_err(),
            "{wrong:?} must refuse the point framing"
        );
    }
}

#[test]
fn the_exempt_verbs_carry_no_format_flag() {
    for argv in [
        &["felis", "--format", "json"][..],
        &["felis", "attach", "1a", "--format", "json"],
        &["felis", "bridge", "--format", "jsonl"],
        &["felis", "completions", "bash", "--format", "json"],
    ] {
        assert!(
            Cli::try_parse_from(argv).is_err(),
            "{argv:?} must not take --format"
        );
    }

    // `felis frontend` forwards `--format` to the frontend untouched
    // rather than refusing it.
    let parsed = Cli::try_parse_from(["felis", "frontend", "tui", "--format", "json"])
        .expect("a frontend's own flags parse as its trailing argv");
    let Some(Cmd::Frontend { name, args }) = parsed.cmd else {
        panic!("`felis frontend tui …` parses as the frontend launch");
    };
    assert_eq!(name, "tui");
    assert_eq!(
        args,
        ["--format", "json"],
        "the flag is forwarded verbatim, not consumed"
    );
}

#[test]
fn the_removed_trace_perf_flag_is_an_unknown_argument() {
    let err = Cli::try_parse_from(["felis", "--trace-perf"])
        .expect_err("--trace-perf must be rejected as unknown");
    assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    let err = Cli::try_parse_from(["felis", "--trace-perf", "sessions", "list"])
        .expect_err("--trace-perf with a verb must be rejected");
    assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    // The flag remains valid when forwarded opaquely after `frontend <name>`.
    let cli = Cli::try_parse_from(["felis", "frontend", "tui", "--trace-perf"])
        .expect("frontend's own --trace-perf forwards opaquely");
    match cli.cmd {
        Some(Cmd::Frontend { name, args }) => {
            assert_eq!(name, "tui");
            assert_eq!(args, vec![OsString::from("--trace-perf")]);
        }
        other => panic!("expected Cmd::Frontend, got {other:?}"),
    }
}

#[test]
fn the_retired_json_flag_is_an_unknown_argument() {
    for argv in [
        &["felis", "sessions", "list", "--jsonl"][..],
        &["felis", "sessions", "info", "--json", "1a"],
        &["felis", "sessions", "capture", "--json", "1a"],
        &["felis", "sessions", "search", "--json", "1a", "pat"],
        &["felis", "sessions", "kill", "--json", "1a"],
        &["felis", "sessions", "evict", "--json", "1a"],
        &["felis", "sessions", "tag", "--json", "1a", "work"],
        &["felis", "sessions", "send", "--json", "--wait", "1a", "x"],
        &["felis", "sessions", "spawn", "--json"],
    ] {
        let err = Cli::try_parse_from(argv)
            .err()
            .unwrap_or_else(|| panic!("{argv:?} must be rejected"));
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::UnknownArgument,
            "{argv:?} must fail as an unknown flag, not as something subtler"
        );
    }
}

#[test]
fn sessions_send_reports_without_a_wait() {
    Cli::try_parse_from(["felis", "sessions", "send", "1a", "x", "--format", "json"])
        .expect("send --format json stands alone");
    Cli::try_parse_from([
        "felis", "sessions", "send", "1a", "x", "--wait", "--format", "json",
    ])
    .expect("send --wait --format json parses");
    // No text: the pure-wait form.
    Cli::try_parse_from([
        "felis", "sessions", "send", "1a", "--wait", "--format", "json",
    ])
    .expect("send --wait with no text parses");
}

#[test]
fn sessions_tag_requires_an_add_or_a_remove() {
    let idle = Cli::try_parse_from(["felis", "sessions", "tag", "1a"]);
    assert!(idle.is_err(), "tag with neither adds nor removes must fail");
    Cli::try_parse_from(["felis", "sessions", "tag", "1a", "--remove", "old"])
        .expect("removes alone parse");
    Cli::try_parse_from([
        "felis", "sessions", "tag", "1a", "new", "--remove", "old", "--remove", "stale",
    ])
    .expect("adds and repeated removes compose");
}

#[test]
fn sessions_capture_format_source_matrix() {
    for argv in [
        &[
            "felis",
            "sessions",
            "capture",
            "--format",
            "jsonl",
            "--source",
            "command-output",
            "1a",
        ][..],
        &[
            "felis",
            "sessions",
            "capture",
            "--format",
            "jsonl",
            "--source",
            "last-command",
            "1a",
        ],
        &[
            "felis", "sessions", "capture", "--format", "jsonl", "--ansi", "1a",
        ],
        &[
            "felis",
            "sessions",
            "capture",
            "--format",
            "jsonl",
            "--ansi",
            "--source",
            "scrollback",
            "1a",
        ],
    ] {
        Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?} must parse: {e}"));
    }
}

#[test]
fn sessions_search_has_no_short_case_insensitive_flag() {
    let short = Cli::try_parse_from(["felis", "sessions", "search", "1a", "pat", "-i"]);
    assert!(short.is_err(), "`-i` must no longer be accepted");
    Cli::try_parse_from([
        "felis",
        "sessions",
        "search",
        "1a",
        "pat",
        "--case-insensitive",
    ])
    .expect("--case-insensitive parses");
}

#[test]
fn sessions_has_no_standalone_wait_verb() {
    let gone = Cli::try_parse_from(["felis", "sessions", "wait", "1a"]);
    assert!(gone.is_err(), "`sessions wait` must not resolve");
}

#[test]
fn cli_attach_is_a_top_level_verb() {
    let cli = Cli::try_parse_from(["felis", "attach", "1a2b"]).expect("parses felis attach 1a2b");
    match cli.cmd {
        Some(Cmd::Attach { id }) => assert_eq!(id, "1a2b"),
        other => panic!("expected Cmd::Attach, got {other:?}"),
    }
}

#[test]
fn the_frontend_verb_passes_its_arguments_through_verbatim() {
    let cli = Cli::try_parse_from(["felis", "frontend", "tui", "attach", "1a2b", "--fullscreen"])
        .expect("parses felis frontend tui …");
    match cli.cmd {
        Some(Cmd::Frontend { name, args }) => {
            assert_eq!(name, "tui");
            assert_eq!(
                args,
                vec![
                    OsString::from("attach"),
                    OsString::from("1a2b"),
                    OsString::from("--fullscreen"),
                ],
            );
        }
        other => panic!("expected Cmd::Frontend, got {other:?}"),
    }
}

#[test]
fn an_unknown_leading_token_is_an_error_not_a_frontend_exec() {
    for argv in [
        &["felis", "ls"][..],
        &["felis", "session", "list"],
        &["felis", "tui", "attach", "1a2b"],
    ] {
        assert!(
            Cli::try_parse_from(argv).is_err(),
            "{argv:?} must be a usage error, not an exec",
        );
    }
}

/// A separator would turn the sibling / `$PATH` lookup of `felis-<name>`
/// into "run this file".
#[test]
fn a_frontend_name_with_a_path_is_rejected() {
    for name in ["../evil", "/usr/bin/evil", "a/b", "./x"] {
        assert!(
            Cli::try_parse_from(["felis", "frontend", name]).is_err(),
            "{name:?} must not parse as a frontend name",
        );
    }
    Cli::try_parse_from(["felis", "frontend", "tui"]).expect("a bare name parses");
}

#[test]
fn global_passthrough_rebuilds_set_flags_only() {
    let none = ConfigSource::Default;
    let cli =
        Cli::try_parse_from(["felis", "--socket", "/tmp/x.sock"]).expect("parses global flags");
    let args = global_passthrough(&cli, &none);
    assert_eq!(
        args,
        vec![OsString::from("--socket"), OsString::from("/tmp/x.sock"),],
    );

    let bare = Cli::try_parse_from(["felis"]).expect("parses bare felis");
    assert_eq!(global_passthrough(&bare, &none), Vec::<OsString>::new());

    let host = Cli::try_parse_from(["felis", "--host", "user@box"]).expect("parses --host");
    assert_eq!(
        global_passthrough(&host, &none),
        vec![OsString::from("--host"), OsString::from("user@box")],
    );

    // One `--ssh-arg` per token: the frontend re-parses them per
    // occurrence, and a joined form cannot be split back apart.
    let ssh_args = Cli::try_parse_from(["felis", "--host", "vm", "--ssh-arg=-p", "--ssh-arg=2222"])
        .expect("parses --ssh-arg");
    assert_eq!(
        global_passthrough(&ssh_args, &none),
        vec![
            OsString::from("--host"),
            OsString::from("vm"),
            OsString::from("--ssh-arg"),
            OsString::from("-p"),
            OsString::from("--ssh-arg"),
            OsString::from("2222"),
        ],
    );
}

/// The window it launches must read the file the user named from
/// *their* directory, and the frontend has no way to know what that
/// was, so only the absolute form crosses the exec.
#[test]
fn a_relative_config_selection_is_forwarded_absolute() {
    let cli = Cli::try_parse_from(["felis", "--config", "felis.toml"]).expect("parses --config");
    let source = resolve_config_source(cli.config.as_deref()).expect("a cwd exists");
    let ConfigSource::Explicit(ref path) = source else {
        panic!("--config must select a file, got {source:?}");
    };
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(path, &cwd.join("felis.toml"));
    assert_eq!(
        global_passthrough(&cli, &source),
        vec![OsString::from("--config"), OsString::from(path)],
    );

    // Built from the cwd rather than written as a literal: `/etc/…` is
    // not absolute on Windows, where this suite also runs.
    let already_absolute = cwd.join("felis.toml");
    let absolute = Cli::try_parse_from([
        OsString::from("felis"),
        OsString::from("--config"),
        already_absolute.clone().into_os_string(),
    ])
    .unwrap();
    assert_eq!(
        resolve_config_source(absolute.config.as_deref()).unwrap(),
        ConfigSource::Explicit(already_absolute),
    );
}

/// `C:felis.toml` names a file on drive C's own current directory,
/// which this process cannot read; the flag is refused rather than
/// forwarded in a form the frontend would resolve differently.
#[cfg(windows)]
#[test]
fn a_drive_relative_config_selection_is_refused() {
    let cli = Cli::try_parse_from(["felis", "--config", "C:felis.toml"]).unwrap();
    let err = resolve_config_source(cli.config.as_deref()).expect_err("cannot be made absolute");
    assert!(err.to_string().contains("drive-relative"), "{err}");
}

/// The flag is root-level like `--socket`, and the verbs that read no
/// config file name themselves in the refusal rather than ignoring it.
#[test]
fn config_selection_is_a_root_flag_the_non_readers_refuse() {
    for argv in [
        vec!["felis", "--config", "c.toml"],
        vec!["felis", "--config", "c.toml", "attach", "1a2b"],
        vec!["felis", "--config", "c.toml", "config", "check"],
        vec!["felis", "--config", "c.toml", "doctor"],
    ] {
        let cli = Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        assert!(
            verb_reading_no_config(cli.cmd.as_ref()).is_none(),
            "{argv:?}"
        );
    }

    // After the command it is not this binary's flag at all.
    assert!(Cli::try_parse_from(["felis", "config", "check", "--config", "c.toml"]).is_err());

    for (argv, verb) in [
        (vec!["felis", "sessions", "list"], "felis sessions"),
        (vec!["felis", "version"], "felis version"),
        (vec!["felis", "bridge"], "felis bridge"),
        (vec!["felis", "completions", "fish"], "felis completions"),
        (vec!["felis", "frontend", "tui"], "felis frontend tui"),
    ] {
        let cli = Cli::try_parse_from(&argv).unwrap();
        let named = verb_reading_no_config(cli.cmd.as_ref())
            .unwrap_or_else(|| panic!("{argv:?} reads no config and must refuse --config"));
        assert_eq!(named, verb);
    }
}

/// What one global flag does to one verb form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Global {
    /// Rebuilt onto the frontend's command line by `global_passthrough`.
    Forwarded,
    /// Named in a usage refusal, exit `2`.
    Refused,
    /// Read by this process; no daemon, no exec.
    InProcess,
    /// Names the daemon this verb connects to.
    Dialed,
    /// `__complete-sessions` only: an empty candidate list, exit `0`.
    NoCandidates,
}

/// The frozen matrix (docs/reference/cli.md "Global options"), as an
/// exhaustive match so a new verb cannot be added without a cell:
/// `(--config, the --host/--socket/--ssh-arg carrier)`.
fn expected_globals(cmd: Option<&Cmd>) -> (Global, Global) {
    use Global::{Dialed, Forwarded, InProcess, NoCandidates, Refused};
    match cmd {
        // The window launches: bare `felis`, `felis -- <cmd>`, `attach`.
        None | Some(Cmd::Attach { .. }) => (Forwarded, Forwarded),
        Some(Cmd::Config { .. }) => (InProcess, Refused),
        Some(Cmd::Doctor { .. }) => (InProcess, Dialed),
        Some(
            Cmd::Sessions { .. }
            | Cmd::Notifications { .. }
            | Cmd::Daemon { .. }
            | Cmd::Version { .. }
            | Cmd::Bridge,
        ) => (Refused, Dialed),
        Some(
            Cmd::Window { .. }
            | Cmd::Ssh { .. }
            | Cmd::Frontend { .. }
            | Cmd::Completions { .. }
            | Cmd::Mangen { .. },
        ) => (Refused, Refused),
        Some(Cmd::CompleteSessions) => (Refused, NoCandidates),
    }
}

/// Every verb form answers every global flag, through production code:
/// the refusals from the dispatch guards, the forwarded set from
/// `global_passthrough`, the dialed and no-candidate cells from the
/// resolver dispatch calls. The two in-process cells are pinned by the
/// `config` and `doctor` integration tests instead.
#[test]
fn every_verb_form_places_every_global_flag() {
    use Global::{Dialed, Forwarded, NoCandidates, Refused};
    use felis_client_core::Carrier;
    for argv in [
        &["felis"][..],
        &["felis", "--", "htop"],
        &["felis", "attach", "1a2b"],
        &["felis", "sessions", "list"],
        &["felis", "notifications", "subscribe"],
        &["felis", "daemon", "status"],
        &["felis", "config", "check"],
        &["felis", "doctor"],
        &["felis", "version"],
        &["felis", "bridge"],
        &["felis", "window", "retarget"],
        &["felis", "ssh", "vm"],
        &["felis", "completions", "fish"],
        &["felis", "__complete-sessions"],
        &["felis", "__mangen", "man"],
        &["felis", "frontend", "tui"],
    ] {
        let cli = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        let (config, carrier) = expected_globals(cli.cmd.as_ref());
        let cmd = cli.cmd.as_ref();

        assert_eq!(
            verb_reading_no_config(cmd).is_some(),
            config == Refused,
            "{argv:?}: --config is {config:?}",
        );
        assert_eq!(
            verb_refusing_carrier(cmd).is_some(),
            carrier == Refused,
            "{argv:?}: the carrier is {carrier:?}",
        );

        // The carrier cells that are not a refusal: `--host` names the
        // daemon a dialing verb reaches, and is dropped by the
        // local-only row rather than reaching an SSH carrier.
        if matches!(carrier, Dialed | NoCandidates) {
            let mut line = vec!["felis", "--host", "vm"];
            line.extend_from_slice(&argv[1..]);
            let with_host = Cli::try_parse_from(&line).unwrap_or_else(|e| panic!("{line:?}: {e}"));
            let placement = dials(with_host.cmd.as_ref());
            assert_eq!(
                placement == Dials::LocalOnly,
                carrier == NoCandidates,
                "{argv:?}: the carrier is {carrier:?}",
            );
            let reached = dial_target(&with_host, placement)
                .ok()
                .map(|target| target.carrier);
            let over_ssh = matches!(
                reached,
                Some(Carrier::Ssh { ref destination, .. }) if destination == "vm"
            );
            assert_eq!(
                over_ssh,
                carrier == Dialed,
                "{argv:?}: --host reached {reached:?}",
            );
        }

        // Only the forms that exec a frontend rebuild the globals for it.
        let execs_a_frontend = matches!(cmd, None | Some(Cmd::Attach { .. }));
        assert_eq!(
            execs_a_frontend,
            config == Forwarded && carrier == Forwarded,
            "{argv:?}: forwarding is all-or-nothing",
        );
        if execs_a_frontend {
            let with_globals = {
                let mut line = vec![
                    "felis",
                    "--host",
                    "vm",
                    "--ssh-arg=-p",
                    "--config",
                    "c.toml",
                ];
                line.extend_from_slice(&argv[1..]);
                Cli::try_parse_from(&line).unwrap_or_else(|e| panic!("{line:?}: {e}"))
            };
            let source =
                resolve_config_source(with_globals.config.as_deref()).expect("a cwd exists");
            let forwarded = global_passthrough(&with_globals, &source);
            for flag in ["--config", "--host", "--ssh-arg"] {
                assert!(
                    forwarded.contains(&OsString::from(flag)),
                    "{argv:?}: {flag} must reach the frontend, got {forwarded:?}",
                );
            }
        }
    }
}

/// `--version` answers before dispatch, so a global on the line would
/// be dropped; each one is a conflict instead.
#[test]
fn the_version_flag_refuses_every_global() {
    let bare = Cli::try_parse_from(["felis", "--version"]).unwrap();
    assert!(version_conflict(&bare).is_none());

    for globals in [
        vec!["--config", "c.toml"],
        vec!["--host", "vm"],
        vec!["--socket", "/tmp/x.sock"],
        vec!["--host", "vm", "--ssh-arg=-p"],
    ] {
        let mut argv = vec!["felis", "--version"];
        argv.extend_from_slice(&globals);
        let cli = Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        let conflict =
            version_conflict(&cli).unwrap_or_else(|| panic!("{argv:?} must be a conflict"));
        assert!(conflict.contains("runs nothing else"), "{conflict}");
    }

    // The pre-existing conflict keeps its own wording.
    let verb = Cli::try_parse_from(["felis", "--version", "sessions", "list"]).unwrap();
    assert!(
        version_conflict(&verb)
            .expect("a verb is a conflict")
            .contains("drop it to run the command"),
    );
}

/// The globals are declared on the root only, so a subcommand's own
/// `--help` lists none of them: the two window-launch verbs say where
/// they go instead of leaving the reader to guess.
#[test]
fn the_launch_verbs_help_says_where_the_globals_go() {
    for (verb, expected) in [
        ("attach", "belong before the verb"),
        ("frontend", "No global reaches it"),
    ] {
        let help = <Cli as clap::CommandFactory>::command()
            .find_subcommand_mut(verb)
            .unwrap_or_else(|| panic!("{verb} is a subcommand"))
            .render_long_help()
            .to_string();
        assert!(help.contains(expected), "{verb} --help: {help}");
        assert!(help.contains("--config"), "{verb} --help: {help}");
        assert!(help.contains("felis --help"), "{verb} --help: {help}");
    }
}

/// `--help` lists the possible values of the finite options.
#[test]
fn help_lists_the_possible_values_of_the_finite_options() {
    let help = <Cli as clap::CommandFactory>::command()
        .find_subcommand_mut("sessions")
        .and_then(|sessions| sessions.find_subcommand_mut("capture"))
        .expect("sessions capture is a subcommand")
        .render_long_help()
        .to_string();
    assert!(help.contains("[possible values: human, jsonl]"), "{help}");
    assert!(
        help.contains("[possible values: visible, scrollback, command-output, last-command]"),
        "{help}"
    );

    let point = <Cli as clap::CommandFactory>::command()
        .find_subcommand_mut("sessions")
        .and_then(|sessions| sessions.find_subcommand_mut("list"))
        .expect("sessions list is a subcommand")
        .render_long_help()
        .to_string();
    assert!(point.contains("[possible values: human, json]"), "{point}");
}

/// Every visible help page in one snapshot.
#[test]
fn every_visible_help_page_is_snapshotted() {
    fn walk(cmd: &mut clap::Command, path: &str, out: &mut String) {
        out.push_str("$ ");
        out.push_str(path);
        out.push_str(" --help\n");
        out.push_str(&cmd.render_long_help().to_string());
        out.push('\n');
        let children: Vec<String> = cmd
            .get_subcommands()
            // clap's own `help` subcommand mirrors the whole tree; its
            // pages are the ones already snapshotted, one indirection
            // out.
            .filter(|sub| !sub.is_hide_set() && sub.get_name() != "help")
            .map(|sub| sub.get_name().to_owned())
            .collect();
        for name in children {
            let child = cmd
                .find_subcommand_mut(&name)
                .unwrap_or_else(|| panic!("{name} is a subcommand"));
            walk(child, &format!("{path} {name}"), out);
        }
    }

    let mut root = <Cli as clap::CommandFactory>::command();
    root.build();
    let mut pages = String::new();
    walk(&mut root, "felis", &mut pages);
    insta::assert_snapshot!(pages);
}

#[test]
fn a_bad_source_value_is_refused_with_the_possible_values() {
    let err = Cli::try_parse_from(["felis", "sessions", "capture", "1a", "--source", "bogus"])
        .expect_err("an unknown --source value is a usage error");
    let rendered = err.to_string();
    assert!(
        rendered.contains("[possible values: visible, scrollback, command-output, last-command]"),
        "{rendered}"
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
fn wrap_exec_err_flags_a_missing_external_frontend_as_unknown() {
    let err = wrap_exec_err(
        "felis-tui",
        std::io::Error::from(std::io::ErrorKind::NotFound),
    );
    let msg = format!("{err}");
    assert!(msg.contains("unknown felis frontend"), "got: {msg}");
    assert!(msg.contains("felis-tui"), "got: {msg}");
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

/// The destination of each retarget verb is its own positional, and
/// the local one is optional: its absence is the way home.
#[test]
fn each_retarget_verb_takes_its_destination_positionally() {
    for argv in [
        &["felis", "window", "retarget"][..], // the default local daemon
        &["felis", "window", "retarget", "/tmp/x.sock"],
        &["felis", "ssh", "vm"],
        &["felis", "ssh", "vm", "--ssh-arg=-p", "--ssh-arg=2222"],
    ] {
        Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?} must parse: {e}"));
    }
    for argv in [
        // The destination is required where it names another daemon.
        &["felis", "ssh"][..],
        // Two destinations, on the verb that takes one.
        &["felis", "window", "retarget", "/tmp/x.sock", "/tmp/y.sock"],
        // A destination is a positional here, never a flag, and
        // `--ssh-arg` belongs to the verb that runs ssh.
        &["felis", "window", "retarget", "--to-host", "vm"],
        &["felis", "window", "retarget", "--to-socket", "/tmp/x.sock"],
        &["felis", "window", "retarget", "--ssh-arg=-p"],
    ] {
        assert!(
            Cli::try_parse_from(argv).is_err(),
            "{argv:?} must be a clap error"
        );
    }
}

/// The socket path and the `-- <cmd>` override are separate slots: a
/// command word must never land in the destination.
#[test]
fn a_retarget_destination_is_not_a_command_word() {
    let cli = Cli::try_parse_from([
        "felis",
        "window",
        "retarget",
        "/tmp/x.sock",
        "--",
        "htop",
        "-d",
        "1",
    ])
    .expect("a path and a command override parse together");
    let Some(Cmd::Window {
        op: WindowOp::Retarget { socket, flags, .. },
    }) = cli.cmd
    else {
        panic!("the window retarget verb parses");
    };
    assert_eq!(socket, Some(PathBuf::from("/tmp/x.sock")));
    assert_eq!(flags.command, vec!["htop", "-d", "1"]);

    let cli = Cli::try_parse_from(["felis", "window", "retarget", "--", "htop"])
        .expect("a command override alone parses");
    let Some(Cmd::Window {
        op: WindowOp::Retarget { socket, flags, .. },
    }) = cli.cmd
    else {
        panic!("the window retarget verb parses");
    };
    assert_eq!(socket, None, "a command word is not a destination");
    assert_eq!(flags.command, vec!["htop"]);
}

/// Every refusal clap can state is clap's, so it lands as a human
/// usage error before a framing is chosen (docs/reference/cli.md
/// "Machine output").
#[test]
fn the_retarget_grammar_is_stated_to_clap() {
    for argv in [
        // Two landings in one request.
        &["felis", "ssh", "vm", "--session", "1a", "--", "htop"][..],
        &[
            "felis",
            "window",
            "retarget",
            "--session",
            "1a",
            "--",
            "htop",
        ],
    ] {
        assert!(
            Cli::try_parse_from(argv).is_err(),
            "{argv:?} must be a clap error, not a post-parse refusal"
        );
    }
    Cli::try_parse_from(["felis", "ssh", "vm", "--session", "1a"]).expect("--session alone parses");
    Cli::try_parse_from(["felis", "ssh", "vm", "--", "htop"])
        .expect("a -- <cmd> override alone parses");
}

#[test]
fn retarget_validates_session_prefixes_at_the_clap_layer() {
    for argv in [
        &["felis", "ssh", "vm", "--session", "nothex"][..],
        &["felis", "ssh", "vm", "--from", "nothex"],
        &["felis", "window", "retarget", "--session", "nothex"],
    ] {
        assert!(
            Cli::try_parse_from(argv).is_err(),
            "{argv:?} must fail: not a hex session prefix"
        );
    }
    Cli::try_parse_from(["felis", "ssh", "vm", "--session", "1a", "--from", "2b"])
        .expect("hex prefixes parse");
}

/// `detach` names the one-window key action, so the verb must not
/// parse.
#[test]
fn the_all_subscriber_disconnect_is_evict_not_detach() {
    assert!(
        Cli::try_parse_from(["felis", "sessions", "detach", "1a"]).is_err(),
        "`sessions detach` must not resolve"
    );
    Cli::try_parse_from(["felis", "sessions", "evict", "1a"]).expect("`sessions evict` parses");
}

/// The auto-spawn matrix (docs/reference/cli.md "Auto-spawning"). The
/// carrier is asserted not to matter: the same verb keeps its policy
/// over a local socket and over SSH, where `Dial::open` hands this
/// policy to the dial that builds the relay command, so `Refuse` is
/// what appends `--no-spawn` to it.
#[test]
fn verb_intent_decides_auto_spawn_on_either_carrier() {
    use felis_client_core::RemoteSpawn::{Allow, Refuse};
    use std::ops::ControlFlow;
    use std::path::Path;

    let verbs: &[(&[&str], felis_client_core::RemoteSpawn)] = &[
        (&["list"], Refuse),
        (&["info", "0123abcd"], Refuse),
        (&["send", "0123abcd", "ls"], Refuse),
        (&["kill", "0123abcd"], Refuse),
        (&["evict", "0123abcd"], Refuse),
        (&["capture", "0123abcd"], Refuse),
        (&["search", "0123abcd", "needle"], Refuse),
        (&["tag", "0123abcd", "work"], Refuse),
        (&["switch", "0123abcd", "--from", "0123abcd"], Refuse),
        (&["spawn"], Allow),
    ];

    let carriers = [
        conn::resolve(None, &[], Some(Path::new("/nonexistent/felis.sock")))
            .expect("a local target resolves")
            .target,
        conn::resolve(Some("user@host"), &[], None)
            .expect("an ssh target resolves")
            .target,
    ];
    for target in &carriers {
        for (argv, expected) in verbs {
            let mut line = vec!["felis", "sessions"];
            line.extend_from_slice(argv);
            let op = match Cli::parse_from(&line).cmd {
                Some(Cmd::Sessions { op }) => op,
                other => panic!("{line:?} is not a sessions verb: {other:?}"),
            };
            let planned = cli_sessions::plan(op, target);
            let ControlFlow::Continue(plan) = planned else {
                panic!("{line:?} was refused before its dial");
            };
            assert_eq!(
                plan.dial.remote_spawn(),
                *expected,
                "{line:?} on {:?}",
                target.carrier
            );
        }
    }

    // The verbs that dial outside `plan`: each names its policy where
    // it dials, so the matrix reads them rather than restating one.
    for (verb, policy) in [
        ("felis daemon", cli_daemon::DIAL.remote_spawn()),
        (
            "felis notifications",
            cli_notifications::DIAL.remote_spawn(),
        ),
        (
            "felis window retarget",
            cli_sessions::RETARGET_DIAL.remote_spawn(),
        ),
        ("felis version", cli_version::REMOTE_SPAWN),
        ("felis doctor", cli_doctor::REMOTE_SPAWN),
        ("felis bridge", cli_bridge::REMOTE_SPAWN),
        ("felis completions", cli_completions::REMOTE_SPAWN),
    ] {
        assert_eq!(policy, Refuse, "{verb} must not resurrect a daemon");
    }
}

/// The frozen argv grammar as a table: one row per line whose outcome
/// the surface has committed to, so a refactor that loosens a rule
/// fails here instead of shipping. This module pins the parser layer;
/// `every_visible_help_page_is_snapshotted` above freezes the
/// generated help pages.
mod argv_matrix {
    use super::*;

    /// What the parser must do with a line. A usage error also carries
    /// the exit code, since `2` is the documented usage-error status
    /// (docs/reference/cli.md "Exit codes").
    #[derive(Debug, Clone, Copy)]
    enum Outcome {
        Parses,
        Usage(clap::error::ErrorKind),
    }

    fn check(argv: &[&str], outcome: Outcome) {
        let parsed = Cli::try_parse_from(argv);
        match outcome {
            Outcome::Parses => {
                parsed.unwrap_or_else(|e| panic!("{argv:?} must parse: {e}"));
            }
            Outcome::Usage(kind) => {
                let err = parsed
                    .err()
                    .unwrap_or_else(|| panic!("{argv:?} must be refused"));
                assert_eq!(err.kind(), kind, "{argv:?} refused as {:?}", err.kind());
                assert_eq!(err.exit_code(), 2, "{argv:?} must exit 2");
            }
        }
    }

    /// A program for `sessions spawn` is only ever the tail after
    /// `--`; without the separator the token is not a program name but
    /// a stray argument.
    #[test]
    fn spawn_takes_its_program_only_after_a_separator() {
        use clap::error::ErrorKind::UnknownArgument;

        for (argv, outcome) in [
            (
                &["felis", "sessions", "spawn", "htop"][..],
                Outcome::Usage(UnknownArgument),
            ),
            (
                &["felis", "sessions", "spawn", "htop", "--flag"],
                Outcome::Usage(UnknownArgument),
            ),
            (
                &["felis", "sessions", "spawn", "--json"],
                Outcome::Usage(UnknownArgument),
            ),
            (&["felis", "sessions", "spawn"], Outcome::Parses),
            (
                &["felis", "sessions", "spawn", "--", "htop"],
                Outcome::Parses,
            ),
        ] {
            check(argv, outcome);
        }
    }

    /// Everything after `--` is the child's, including spellings felis
    /// itself owns: the separator, not the vocabulary, decides who
    /// consumes a token.
    #[test]
    fn only_the_flags_before_the_separator_are_felis_own() {
        let cli = Cli::try_parse_from([
            "felis", "sessions", "spawn", "--format", "json", "--", "htop", "--format", "json",
        ])
        .expect("felis's own --format precedes the separator");
        let Some(Cmd::Sessions {
            op: cli_sessions::SessionOp::Spawn { cmd, output, .. },
        }) = cli.cmd
        else {
            panic!("expected `sessions spawn`");
        };
        assert_eq!(cmd, ["htop", "--format", "json"]);
        assert_eq!(output.format, cli_output::Format::Json);
    }

    /// Profiling and framing flags absent from the grammar are unknown
    /// spellings, not silently ignored.
    #[test]
    fn the_removed_flags_stay_unknown_arguments() {
        use clap::error::ErrorKind::UnknownArgument;

        for argv in [
            &["felis", "--trace-perf"][..],
            &["felis", "--trace-perf", "sessions", "list"],
            &["felis", "sessions", "spawn", "--trace-perf"],
            &["felis", "sessions", "list", "--json"],
        ] {
            check(argv, Outcome::Usage(UnknownArgument));
        }
    }

    /// A framing outside the verb's class is an invalid value, so the
    /// refusal can list what the verb does take.
    #[test]
    fn a_cross_class_framing_is_an_invalid_value() {
        use clap::error::ErrorKind::InvalidValue;

        for argv in [
            &["felis", "sessions", "list", "--format", "jsonl"][..],
            &["felis", "sessions", "spawn", "--format", "jsonl"],
            &["felis", "sessions", "capture", "1a", "--format", "json"],
            &["felis", "notifications", "subscribe", "--format", "json"],
        ] {
            check(argv, Outcome::Usage(InvalidValue));
        }

        let err = Cli::try_parse_from(["felis", "sessions", "list", "--format", "jsonl"])
            .expect_err("jsonl is not a point framing");
        assert!(
            err.to_string().contains("[possible values: human, json]"),
            "{err}"
        );
    }

    /// `frontend <name>` is the one place a token felis does not know
    /// crosses the surface; every other unknown leading token is a
    /// usage error rather than an exec of something on `$PATH`.
    #[test]
    fn no_verb_but_frontend_passes_an_unknown_token_through() {
        use clap::error::ErrorKind::UnknownArgument;

        for (argv, outcome) in [
            (&["felis", "htop"][..], Outcome::Usage(UnknownArgument)),
            (&["felis", "--fullscreen"], Outcome::Usage(UnknownArgument)),
            (
                &["felis", "frontend", "tui", "--fullscreen"],
                Outcome::Parses,
            ),
        ] {
            check(argv, outcome);
        }
    }

    /// The whole `send` grammar: at least one of TEXT / `--key` /
    /// `--wait`, `--raw` only with TEXT, `--timeout` only with
    /// `--wait` (docs/reference/cli.md "Verb details").
    #[test]
    fn send_takes_three_constraints_and_no_others() {
        use clap::error::ErrorKind::MissingRequiredArgument;

        for (argv, outcome) in [
            // No payload of any kind names no operation.
            (
                &["felis", "sessions", "send", "1a"][..],
                Outcome::Usage(MissingRequiredArgument),
            ),
            // `--timeout` bounds a wait, so it needs one to bound.
            (
                &["felis", "sessions", "send", "1a", "x", "--timeout", "5"],
                Outcome::Usage(MissingRequiredArgument),
            ),
            // `--raw` describes how TEXT travels.
            (
                &["felis", "sessions", "send", "--raw"],
                Outcome::Usage(MissingRequiredArgument),
            ),
            (
                &["felis", "sessions", "send", "1a", "--raw"],
                Outcome::Usage(MissingRequiredArgument),
            ),
            // Each payload kind stands alone, and they compose.
            (
                &["felis", "sessions", "send", "1a", "--key", "enter"],
                Outcome::Parses,
            ),
            (
                &["felis", "sessions", "send", "1a", "--wait"],
                Outcome::Parses,
            ),
            (
                &[
                    "felis",
                    "sessions",
                    "send",
                    "1a",
                    "--wait",
                    "--timeout",
                    "5",
                ],
                Outcome::Parses,
            ),
            (
                &["felis", "sessions", "send", "1a", "x", "--raw"],
                Outcome::Parses,
            ),
            (
                &[
                    "felis", "sessions", "send", "1a", "x", "--key", "enter", "--wait",
                ],
                Outcome::Parses,
            ),
        ] {
            check(argv, outcome);
        }
    }
}

/// A `--timeout` whose deadline overflows `Instant` waits without one
/// instead of panicking; a representable one still yields a deadline.
#[test]
fn timeout_deadline_treats_an_unrepresentable_timeout_as_none() {
    assert_eq!(timeout_deadline(None), None);
    assert_eq!(timeout_deadline(Some(u64::MAX)), None);
    let before = tokio::time::Instant::now();
    let deadline = timeout_deadline(Some(0)).expect("a zero timeout is a deadline");
    assert!(deadline >= before);
}

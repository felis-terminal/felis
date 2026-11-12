use super::*;

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

#[test]
fn global_passthrough_rebuilds_the_window_class_after_the_carrier() {
    let cli = Cli::try_parse_from(["felis", "--class", "scratch", "--socket", "/tmp/x.sock"])
        .expect("parses --class");
    assert_eq!(
        global_passthrough(&cli, &ConfigSource::Default),
        ["--socket", "/tmp/x.sock", "--class", "scratch"].map(OsString::from),
    );
}

/// The shared validator runs at felis's own parse, so a name the
/// frontend would refuse never crosses the exec.
#[test]
fn an_invalid_window_class_is_a_usage_error_before_any_verb_runs() {
    let too_long = "a".repeat(256);
    for value in ["", "a\tb", "a\u{1b}b", too_long.as_str()] {
        let err = Cli::try_parse_from(["felis", "--class", value])
            .expect_err("an invalid class must not parse");
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::ValueValidation,
            "{value:?}"
        );
        assert_eq!(err.exit_code(), 2);
    }
    let longest = "a".repeat(255);
    for value in [longest.as_str(), "端末", "felis scratch"] {
        let cli = Cli::try_parse_from(["felis", "--class", value])
            .unwrap_or_else(|e| panic!("{value:?}: {e}"));
        assert_eq!(cli.class.as_deref(), Some(value));
    }
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
        (vec!["felis", "completions", "fish"], "felis completions"),
        (vec!["felis", "fonts", "list"], "felis fonts"),
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
/// `(--config, the --host/--socket/--ssh-arg carrier, --class)`.
fn expected_globals(cmd: Option<&Cmd>) -> (Global, Global, Global) {
    use Global::{Dialed, Forwarded, InProcess, NoCandidates, Refused};
    match cmd {
        // The window launches: bare `felis`, `felis -- <cmd>`, `attach`.
        None | Some(Cmd::Attach { .. }) => (Forwarded, Forwarded, Forwarded),
        Some(Cmd::Config { .. }) => (InProcess, Refused, Refused),
        Some(Cmd::Doctor { .. }) => (InProcess, Dialed, Refused),
        Some(
            Cmd::Sessions { .. }
            | Cmd::Notifications { .. }
            | Cmd::Daemon { .. }
            | Cmd::Version { .. },
        ) => (Refused, Dialed, Refused),
        Some(
            Cmd::Window { .. }
            | Cmd::Ssh { .. }
            | Cmd::Fonts { .. }
            | Cmd::Completions { .. }
            | Cmd::Mangen { .. },
        ) => (Refused, Refused, Refused),
        Some(Cmd::CompleteSessions) => (Refused, NoCandidates, Refused),
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
        &["felis", "window", "retarget"],
        &["felis", "ssh", "vm"],
        &["felis", "fonts", "list"],
        &["felis", "completions", "fish"],
        &["felis", "__complete-sessions"],
        &["felis", "__mangen", "man"],
    ] {
        let cli = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        let (config, carrier, class) = expected_globals(cli.cmd.as_ref());
        let cmd = cli.cmd.as_ref();

        assert_eq!(
            verb_opening_no_window(cmd).is_some(),
            class == Refused,
            "{argv:?}: --class is {class:?}",
        );

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
            config == Forwarded && carrier == Forwarded && class == Forwarded,
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
                    "--class",
                    "scratch",
                ];
                line.extend_from_slice(&argv[1..]);
                Cli::try_parse_from(&line).unwrap_or_else(|e| panic!("{line:?}: {e}"))
            };
            let source =
                resolve_config_source(with_globals.config.as_deref()).expect("a cwd exists");
            let forwarded = global_passthrough(&with_globals, &source);
            for flag in ["--config", "--host", "--ssh-arg", "--class"] {
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
        vec!["--class", "scratch"],
    ] {
        let mut argv = vec!["felis", "--version"];
        argv.extend_from_slice(&globals);
        let cli = Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        let conflict =
            version_conflict(&cli).unwrap_or_else(|| panic!("{argv:?} must be a conflict"));
        assert!(conflict.contains("runs nothing else"), "{conflict}");
    }

    let verb = Cli::try_parse_from(["felis", "--version", "sessions", "list"]).unwrap();
    assert!(
        version_conflict(&verb)
            .expect("a verb is a conflict")
            .contains("drop it to run the command"),
    );
}

/// The globals are declared on the root only, so a subcommand's own
/// `--help` lists none of them: the window-launch verb says where they
/// go instead of leaving the reader to guess.
#[test]
fn the_launch_verb_help_says_where_the_globals_go() {
    let help = <Cli as clap::CommandFactory>::command()
        .find_subcommand_mut("attach")
        .expect("attach is a subcommand")
        .render_long_help()
        .to_string();
    assert!(help.contains("belong before the verb"), "{help}");
    assert!(help.contains("--config"), "{help}");
    assert!(help.contains("felis --help"), "{help}");
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
        ("felis completions", cli_completions::REMOTE_SPAWN),
    ] {
        assert_eq!(policy, Refuse, "{verb} must not resurrect a daemon");
    }
}

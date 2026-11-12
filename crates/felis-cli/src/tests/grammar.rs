use super::*;

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
        &["felis", "completions", "bash", "--format", "json"],
    ] {
        assert!(
            Cli::try_parse_from(argv).is_err(),
            "{argv:?} must not take --format"
        );
    }

    // An external command gets `--format` untouched rather than refused.
    let external = external_command(&argv(&["felis", "tui", "--format", "json"]))
        .expect("an unknown word is an external command");
    assert_eq!(
        external.args,
        argv(&["--format", "json"]),
        "the flag is forwarded verbatim, not consumed"
    );
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
            &["felis", "sessions", "list", "--jsonl"],
            &["felis", "sessions", "info", "--json", "1a"],
            &["felis", "sessions", "capture", "--json", "1a"],
            &["felis", "sessions", "search", "--json", "1a", "pat"],
            &["felis", "sessions", "kill", "--json", "1a"],
            &["felis", "sessions", "evict", "--json", "1a"],
            &["felis", "sessions", "tag", "--json", "1a", "work"],
            &["felis", "sessions", "send", "--json", "--wait", "1a", "x"],
        ] {
            check(argv, Outcome::Usage(UnknownArgument));
        }
    }

    /// `capture`'s framing, source, and `--ansi` compose freely; only a
    /// source outside the closed set is refused.
    #[test]
    fn capture_composes_its_framing_and_source_flags() {
        use clap::error::ErrorKind::InvalidValue;

        for (argv, outcome) in [
            (
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
                Outcome::Parses,
            ),
            (
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
                Outcome::Parses,
            ),
            (
                &[
                    "felis", "sessions", "capture", "--format", "jsonl", "--ansi", "1a",
                ],
                Outcome::Parses,
            ),
            (
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
                Outcome::Parses,
            ),
            (
                &["felis", "sessions", "capture", "1a", "--source", "bogus"],
                Outcome::Usage(InvalidValue),
            ),
        ] {
            check(argv, outcome);
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
    }

    /// Clap alone never reaches an exec: an unknown leading token is
    /// a usage error to it, and only `external_command`, which runs
    /// first, turns it into a `felis-<name>` lookup.
    #[test]
    fn clap_refuses_an_unknown_leading_token() {
        use clap::error::ErrorKind::UnknownArgument;

        for argv in [&["felis", "htop"][..], &["felis", "--fullscreen"]] {
            check(argv, Outcome::Usage(UnknownArgument));
        }
    }

    /// The whole `send` grammar: at least one of TEXT / `--key` /
    /// `--mouse` / `--wait`, `--raw` only with TEXT, `--timeout` only with
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
                &["felis", "sessions", "send", "1a", "--mouse", "click:0,0"],
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
            // A machine framing reports on its own, with or without a wait.
            (
                &["felis", "sessions", "send", "1a", "x", "--format", "json"],
                Outcome::Parses,
            ),
            (
                &[
                    "felis", "sessions", "send", "1a", "x", "--wait", "--format", "json",
                ],
                Outcome::Parses,
            ),
            (
                &[
                    "felis", "sessions", "send", "1a", "--wait", "--format", "json",
                ],
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

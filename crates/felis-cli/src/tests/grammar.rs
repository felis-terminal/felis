//! The frozen argv grammar: one row per line whose outcome the surface
//! has committed to, so a refactor that loosens a rule fails here
//! instead of shipping. The `help` module freezes the generated help
//! pages.

use clap::error::ErrorKind::{
    ArgumentConflict, InvalidSubcommand, InvalidValue, MissingRequiredArgument, UnknownArgument,
    ValueValidation,
};

use super::*;

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
    check(
        &["felis", "sessions", "send", "--from-stdin", "1a", "x"],
        Outcome::Usage(UnknownArgument),
    );
    check(&["felis", "sessions", "send", "1a", "-"], Outcome::Parses);
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
        check(argv, Outcome::Parses);
    }
    check(
        &["felis", "daemon", "stop", "--force", "--when-empty"],
        Outcome::Usage(ArgumentConflict),
    );
}

#[test]
fn sessions_spawn_rejects_malformed_env_at_the_clap_layer() {
    check(
        &["felis", "sessions", "spawn", "--env", "NOEQUALS"],
        Outcome::Usage(ValueValidation),
    );
    check(
        &["felis", "sessions", "spawn", "--env", "KEY=VAL"],
        Outcome::Parses,
    );
}

/// The point/stream classification is normative (docs/reference/cli.md
/// "Machine output"), so it is pinned per verb rather than sampled. A
/// framing outside the verb's class is an invalid value, so the refusal
/// can list what the verb does take.
#[test]
fn each_verb_accepts_only_its_own_class_format() {
    let point_verbs = [
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
    ];
    let stream_verbs = [
        &["felis", "sessions", "capture", "1a"][..],
        &["felis", "sessions", "search", "1a", "pat"],
        &["felis", "notifications", "subscribe"],
    ];
    let with = |argv: &[&'static str], format: &'static str| -> Vec<&'static str> {
        argv.iter().copied().chain(["--format", format]).collect()
    };
    for (verbs, own, other) in [
        (&point_verbs[..], "json", "jsonl"),
        (&stream_verbs[..], "jsonl", "json"),
    ] {
        for argv in verbs {
            check(&with(argv, "human"), Outcome::Parses);
            check(&with(argv, own), Outcome::Parses);
            check(&with(argv, other), Outcome::Usage(InvalidValue));
        }
    }
}

#[test]
fn the_exempt_verbs_carry_no_format_flag() {
    for argv in [
        &["felis", "--format", "json"][..],
        &["felis", "attach", "1a", "--format", "json"],
        &["felis", "completions", "bash", "--format", "json"],
    ] {
        check(argv, Outcome::Usage(UnknownArgument));
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
    check(
        &["felis", "sessions", "tag", "1a"],
        Outcome::Usage(MissingRequiredArgument),
    );
    check(
        &["felis", "sessions", "tag", "1a", "--remove", "old"],
        Outcome::Parses,
    );
    check(
        &[
            "felis", "sessions", "tag", "1a", "new", "--remove", "old", "--remove", "stale",
        ],
        Outcome::Parses,
    );
}

#[test]
fn sessions_search_has_no_short_case_insensitive_flag() {
    check(
        &["felis", "sessions", "search", "1a", "pat", "-i"],
        Outcome::Usage(UnknownArgument),
    );
    check(
        &[
            "felis",
            "sessions",
            "search",
            "1a",
            "pat",
            "--case-insensitive",
        ],
        Outcome::Parses,
    );
}

#[test]
fn sessions_wait_is_not_a_verb() {
    check(
        &["felis", "sessions", "wait", "1a"],
        Outcome::Usage(InvalidSubcommand),
    );
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
        check(argv, Outcome::Parses);
    }
    for (argv, kind) in [
        // The destination is required where it names another daemon.
        (&["felis", "ssh"][..], MissingRequiredArgument),
        // Two destinations, on the verb that takes one.
        (
            &["felis", "window", "retarget", "/tmp/x.sock", "/tmp/y.sock"],
            UnknownArgument,
        ),
        // A destination is a positional here, never a flag, and
        // `--ssh-arg` belongs to the verb that runs ssh.
        (
            &["felis", "window", "retarget", "--to-host", "vm"],
            UnknownArgument,
        ),
        (
            &["felis", "window", "retarget", "--to-socket", "/tmp/x.sock"],
            UnknownArgument,
        ),
        (
            &["felis", "window", "retarget", "--ssh-arg=-p"],
            UnknownArgument,
        ),
    ] {
        check(argv, Outcome::Usage(kind));
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
        check(argv, Outcome::Usage(ArgumentConflict));
    }
    check(&["felis", "ssh", "vm", "--session", "1a"], Outcome::Parses);
    check(&["felis", "ssh", "vm", "--", "htop"], Outcome::Parses);
}

#[test]
fn retarget_validates_session_prefixes_at_the_clap_layer() {
    for argv in [
        &["felis", "ssh", "vm", "--session", "nothex"][..],
        &["felis", "ssh", "vm", "--from", "nothex"],
        &["felis", "window", "retarget", "--session", "nothex"],
    ] {
        check(argv, Outcome::Usage(ValueValidation));
    }
    check(
        &["felis", "ssh", "vm", "--session", "1a", "--from", "2b"],
        Outcome::Parses,
    );
}

/// `detach` names the one-window key action, so the verb must not
/// parse.
#[test]
fn the_all_subscriber_disconnect_is_evict_not_detach() {
    check(
        &["felis", "sessions", "detach", "1a"],
        Outcome::Usage(InvalidSubcommand),
    );
    check(&["felis", "sessions", "evict", "1a"], Outcome::Parses);
}

/// A program for `sessions spawn` is only ever the tail after
/// `--`; without the separator the token is not a program name but
/// a stray argument.
#[test]
fn spawn_takes_its_program_only_after_a_separator() {
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
fn an_unsupported_framing_or_profiling_flag_is_an_unknown_argument() {
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

/// Clap alone never reaches an exec: an unknown leading token is
/// a usage error to it, and only `external_command`, which runs
/// first, turns it into a `felis-<name>` lookup.
#[test]
fn clap_refuses_an_unknown_leading_token() {
    for argv in [&["felis", "htop"][..], &["felis", "--fullscreen"]] {
        check(argv, Outcome::Usage(UnknownArgument));
    }
}

/// The whole `send` grammar: at least one of TEXT / `--key` /
/// `--mouse` / `--wait`, `--raw` only with TEXT, `--timeout` only with
/// `--wait` (docs/reference/cli.md "Verb details").
#[test]
fn send_takes_three_constraints_and_no_others() {
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

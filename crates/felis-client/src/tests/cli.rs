use super::*;

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

#[test]
fn cli_flags_before_double_dash_do_not_leak_into_command() {
    let cli = Cli::try_parse_from(["felis", "--socket", "/tmp/s.sock", "--", "ls"])
        .expect("parses felis --socket P -- ls");
    assert_eq!(cli.socket.as_deref(), Some(Path::new("/tmp/s.sock")));
    assert_eq!(cli.command, vec!["ls"]);
}

/// The front-door `felis` answers a typo the same way.
#[test]
fn cli_bare_token_without_double_dash_is_a_subcommand_not_a_command() {
    assert!(Cli::try_parse_from(["felis-client", "definitely-not-a-subcommand"]).is_err());
    let cli = Cli::try_parse_from(["felis-client", "attach", "1a2b"]).expect("parses attach");
    assert!(matches!(cli.cmd, Some(ClientCmd::Attach { .. })));
    assert_eq!(cli.command, Vec::<String>::new());
}

#[test]
fn perf_trace_directive_includes_hot_path_targets() {
    let perf = log_filter_directive(true);
    assert!(perf.contains("felis::redraw=trace"), "{perf}");
    assert!(perf.contains("felis::pull=trace"), "{perf}");
    let normal = log_filter_directive(false);
    assert!(
        !normal.contains("=trace"),
        "default must not enable trace: {normal}"
    );
    assert!(normal.contains("felis=debug"));
}

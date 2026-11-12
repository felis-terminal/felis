use super::*;

/// security-model.md "Process and environment boundary": inherit minus the
/// denylist, no allowlist, and `SpawnArgs.env` wins over the identity
/// stamps (REQ-912).
#[test]
fn spawn_args_command_inherits_daemon_env_with_user_overrides_last() {
    let session_id = 0xfe115_u128;
    let args = felis_protocol::messages::SpawnArgs {
        command: "printenv".to_owned(),
        env: vec![("TERM".to_owned(), "user-chosen-term".to_owned())],
        ..Default::default()
    };
    let factory: SessionFactory =
        Arc::new(|_| panic!("a named command must not reach the default-program factory"));
    let cmd = command_from_args(&args, &factory, session_id, &ResolvedEnv::birth(), None);
    // Keys are stored folded (uppercase on Windows), so probe keys fold the
    // same way.
    let fold = |k: &OsStr| -> String {
        let s = k.to_string_lossy();
        if cfg!(windows) {
            s.to_ascii_uppercase()
        } else {
            s.into_owned()
        }
    };
    let envs: std::collections::BTreeMap<String, OsString> = cmd
        .get_envs()
        .map(|(k, v)| (fold(k), v.to_os_string()))
        .collect();

    // A test run inside a felis window inherits TERM_PROGRAM=felis, so the
    // stamps are asserted on their own values.
    let stamped = [
        "TERM",
        "COLORTERM",
        "TERM_PROGRAM",
        "TERM_PROGRAM_VERSION",
        "FELIS_SESSION_ID",
        "FELIS_SOCKET",
    ];
    for (key, val) in std::env::vars_os() {
        let name = fold(&key);
        if crate::ENV_DENYLIST.contains(&name.as_str())
            || crate::ORIGIN_ENV.contains(&name.as_str())
        {
            assert!(
                !envs.contains_key(&name),
                "scrubbed var {name} leaked into the built command",
            );
        } else if !stamped.contains(&name.as_str()) {
            assert_eq!(
                envs.get(&name).map(OsString::as_os_str),
                Some(val.as_os_str()),
                "inherited var {name} did not reach the built command",
            );
        }
    }
    assert_eq!(
        envs.get("TERM").map(OsString::as_os_str),
        Some(OsStr::new("user-chosen-term")),
    );
    let sid = format!("{session_id:032x}");
    assert_eq!(
        envs.get("FELIS_SESSION_ID").map(OsString::as_os_str),
        Some(OsStr::new(sid.as_str())),
    );
    assert!(
        !envs.contains_key("FELIS_SOCKET"),
        "FELIS_SOCKET must not leak from the parent when the daemon has no endpoint",
    );
}

#[test]
fn spawn_args_set_the_current_origin_over_a_stale_inherited_one() {
    let mut cmd = Command::new("true");
    cmd.env("FELIS_HOST", "stale@remote");
    cmd.env("FELIS_CWD", "file://remote/stale");
    cmd.env("FELIS_ORIGIN_SESSION_ID", "stale");
    apply_spawn_posture(
        &mut cmd,
        1,
        &ResolvedEnv::birth(),
        &[("FELIS_ORIGIN_SESSION_ID".to_owned(), "current".to_owned())],
        None,
    );
    assert_eq!(
        env_of(&cmd, "FELIS_ORIGIN_SESSION_ID").as_deref(),
        Some("current")
    );
    assert_eq!(env_of(&cmd, "FELIS_HOST"), None);
    assert_eq!(env_of(&cmd, "FELIS_CWD"), None);
}

#[cfg(unix)]
#[test]
fn the_terminfo_dir_leads_a_terminfo_dirs_the_spawn_request_sets() {
    let mut cmd = Command::new("true");
    apply_spawn_posture(
        &mut cmd,
        1,
        &ResolvedEnv::birth(),
        &[("TERMINFO_DIRS".to_owned(), "/caller/terminfo".to_owned())],
        Some(Path::new("/opt/felis/share/terminfo")),
    );
    assert_eq!(
        cmd.get_envs()
            .find(|(key, _)| *key == "TERMINFO_DIRS")
            .map(|(_, value)| value.to_os_string()),
        Some(OsString::from("/opt/felis/share/terminfo:/caller/terminfo")),
    );
}

#[test]
fn no_terminfo_dir_leaves_the_requested_terminfo_dirs_as_it_is() {
    let mut cmd = Command::new("true");
    apply_spawn_posture(
        &mut cmd,
        1,
        &ResolvedEnv::birth(),
        &[("TERMINFO_DIRS".to_owned(), "/caller/terminfo".to_owned())],
        None,
    );
    assert_eq!(
        cmd.get_envs()
            .find(|(key, _)| *key == "TERMINFO_DIRS")
            .map(|(_, value)| value.to_os_string()),
        Some(OsString::from("/caller/terminfo")),
    );
}

/// The embedder's directory, not the one beside the test binary, reaches
/// the child, on the plain create and on the bare launch retried after an
/// unusable cwd.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_caps_terminfo_dir_reaches_a_created_session_and_a_cwd_retry() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let terminfo = tmp.path().join("ti");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory(r#"printf 'TI=%s|' "$TERMINFO_DIRS"; sleep 0.5"#);
    let caps = DaemonCaps {
        terminfo_dir: Some(terminfo.clone()),
        ..DaemonCaps::default()
    };
    spawn_daemon_with_caps(&path, pool, factory, caps);
    let expected = format!("TI={}:|", terminfo.display());

    for cwd in ["", "/felis-no-such-directory-for-tests"] {
        let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
        send_kind(
            &mut writer,
            &SessionToDaemonMsg::Create {
                args: felis_protocol::messages::SpawnArgs {
                    cwd: cwd.to_owned(),
                    ..Default::default()
                },
                image_regions: false,
            },
        )
        .await;
        expect_row_containing(&mut reader, &expected).await;
    }
}

/// `FELIS_SESSION_ID` or a denylisted key in `SpawnArgs.env` is refused
/// with the key in the reason (REQ-912).
#[test]
fn spawn_with_args_rejects_a_reserved_env_key() {
    let factory: SessionFactory =
        Arc::new(|_| panic!("the default factory must not run for an invalid SpawnArgs"));
    for key in ["FELIS_SESSION_ID", "VTE_VERSION"] {
        let args = felis_protocol::messages::SpawnArgs {
            env: vec![(key.to_owned(), "x".to_owned())],
            ..Default::default()
        };
        match spawn_with_args(
            &args,
            &factory,
            0,
            EnvBaseSource::Birth,
            None,
            None,
            None,
            crate::DEFAULT_SIZE,
        ) {
            Ok(_) => panic!("reserved env key {key} must be rejected"),
            Err(err) => assert!(
                err.to_string().contains(key) && err.to_string().contains("reserved"),
                "the refusal reason must name the reserved key felis owns, got: {err}"
            ),
        }
    }
}

/// Creation-time tags obey the `OpsToDaemonMsg::Tag` caps.
#[test]
fn spawn_with_args_rejects_an_over_cap_tag_set() {
    let factory: SessionFactory =
        Arc::new(|_| panic!("the default factory must not run for an invalid SpawnArgs"));
    let args = felis_protocol::messages::SpawnArgs {
        tags: (0..=felis_protocol::messages::MAX_SESSION_TAGS)
            .map(|i| format!("t{i}"))
            .collect(),
        ..Default::default()
    };
    match spawn_with_args(
        &args,
        &factory,
        0,
        EnvBaseSource::Birth,
        None,
        None,
        None,
        crate::DEFAULT_SIZE,
    ) {
        Ok(_) => panic!("an over-cap tag set must be rejected"),
        Err(err) => assert!(
            err.to_string().contains("at most"),
            "the refusal reason must name the cap, got: {err}"
        ),
    }
}

/// Args without a command are refused with a reason rather than reaching
/// `Command::new("")` and an opaque ENOENT.
#[test]
fn spawn_with_args_rejects_args_without_a_command() {
    let factory: SessionFactory =
        Arc::new(|_| panic!("the default factory must not run for an invalid SpawnArgs"));
    let args = felis_protocol::messages::SpawnArgs {
        command: String::new(),
        args: vec!["-l".to_owned()],
        ..Default::default()
    };
    // `Session` is not `Debug`, so match rather than `expect_err`.
    match spawn_with_args(
        &args,
        &factory,
        0,
        EnvBaseSource::Birth,
        None,
        None,
        None,
        crate::DEFAULT_SIZE,
    ) {
        Ok(_) => panic!("args without a command must be rejected"),
        Err(err) => assert!(
            err.to_string()
                .contains("SpawnArgs: args without a command"),
            "the refusal reason must name the problem, got: {err}"
        ),
    }
}

/// The fallback chain: a present snapshot over the relay's, the relay's
/// over the daemon's own.
#[test]
fn the_env_base_chain_picks_request_then_relay_then_birth() {
    let request = vec![entry("FROM", "request")];
    let relay: Vec<crate::child_env::EnvEntry> = vec![entry("FROM", "relay")];

    let with_base = felis_protocol::messages::SpawnArgs {
        env_base: Some(request),
        ..Default::default()
    };
    assert!(matches!(
        env_base_source(&with_base, Some(&relay)),
        EnvBaseSource::Request(_)
    ));

    let bare = felis_protocol::messages::SpawnArgs::default();
    assert!(matches!(
        env_base_source(&bare, Some(&relay)),
        EnvBaseSource::Relay(_)
    ));
    assert!(matches!(env_base_source(&bare, None), EnvBaseSource::Birth));

    // Presence, not emptiness: an empty snapshot is still the client's
    // answer.
    let empty = felis_protocol::messages::SpawnArgs {
        env_base: Some(Vec::new()),
        ..Default::default()
    };
    assert!(matches!(
        env_base_source(&empty, Some(&relay)),
        EnvBaseSource::Request(_)
    ));
}

fn resolved_request(base: &[crate::child_env::EnvEntry]) -> ResolvedEnv {
    resolve_env_base(
        EnvBaseSource::Request(base),
        None,
        None,
        crate::child_env::TARGET_IS_WINDOWS,
    )
    .expect("a representable base resolves")
}

/// A base replaces the daemon's environment rather than layering over it:
/// an overlay would keep the stale entries the base displaces.
#[test]
fn a_base_replaces_the_daemons_environment() {
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = Command::new("/bin/sh");
        cmd.env("DAEMON_ONLY", "stale");
        cmd
    });
    let args = felis_protocol::messages::SpawnArgs::default();
    let base = vec![entry("FROM_BASE", "fresh")];

    let replaced = command_from_args(&args, &factory, 0, &resolved_request(&base), None);
    assert_eq!(env_of(&replaced, "FROM_BASE").as_deref(), Some("fresh"));
    assert_eq!(
        env_of(&replaced, "DAEMON_ONLY"),
        None,
        "nothing from the daemon's own environment survives a base"
    );

    let birth = command_from_args(&args, &factory, 0, &ResolvedEnv::birth(), None);
    assert_eq!(env_of(&birth, "DAEMON_ONLY").as_deref(), Some("stale"));
}

/// Order: base, denylist scrub, identity stamps, explicit `env` last
/// (REQ-912).
#[test]
fn the_order_is_base_then_scrub_then_stamps_then_explicit() {
    let factory: SessionFactory = Arc::new(|_| Command::new("/bin/sh"));
    let args = felis_protocol::messages::SpawnArgs {
        env: vec![("TERM".to_owned(), "user-chosen".to_owned())],
        ..Default::default()
    };
    let base = vec![
        entry("VTE_VERSION", "6003"),
        entry("FELIS_SESSION_ID", "stale"),
        entry("TERM", "from-base"),
        entry("COLORTERM", "from-base"),
        entry("KEEP", "kept"),
    ];

    let cmd = command_from_args(&args, &factory, 0xabc, &resolved_request(&base), None);
    assert_eq!(env_of(&cmd, "KEEP").as_deref(), Some("kept"));
    assert_eq!(
        env_of(&cmd, "VTE_VERSION"),
        None,
        "a denylisted key is scrubbed out of an inherited base, silently"
    );
    assert_eq!(
        env_of(&cmd, "FELIS_SESSION_ID").as_deref(),
        Some(&*hex(0xabc)),
        "the identity stamp is felis's, never the base's"
    );
    assert_eq!(
        env_of(&cmd, "COLORTERM").as_deref(),
        Some("truecolor"),
        "a stamp overrides the base"
    );
    assert_eq!(
        env_of(&cmd, "TERM").as_deref(),
        Some("user-chosen"),
        "an explicit pair overrides even a stamp (REQ-912)"
    );
}

/// `$SHELL` is part of the environment answer: a warm daemon's names
/// whichever login auto-spawned it.
#[test]
fn the_default_program_takes_the_shell_the_base_resolved() {
    let base = vec![entry("SHELL", "/usr/bin/somesh")];
    assert_eq!(
        SpawnedPty::default_shell_command(Some(&base)).get_program(),
        OsStr::new("/usr/bin/somesh")
    );

    let fallback =
        std::env::var_os("SHELL").unwrap_or_else(|| OsString::from(crate::default_shell()));
    assert_eq!(
        SpawnedPty::default_shell_command(Some(&[])).get_program(),
        fallback
    );
    assert_eq!(
        SpawnedPty::default_shell_command(None).get_program(),
        fallback
    );
}

/// The identity escape hatch is read from the environment the stamp is
/// applied over and never reaches the child (denylisted).
#[test]
fn the_identity_hatch_is_read_from_the_resolved_base() {
    let factory: SessionFactory = Arc::new(|_| Command::new("/bin/sh"));
    let args = felis_protocol::messages::SpawnArgs::default();
    let base = vec![
        entry("FELIS_TERM", "xterm-kitty"),
        entry("FELIS_TERM_PROGRAM", "kitty"),
    ];

    let cmd = command_from_args(&args, &factory, 0, &resolved_request(&base), None);
    assert_eq!(env_of(&cmd, "TERM").as_deref(), Some("xterm-kitty"));
    assert_eq!(env_of(&cmd, "TERM_PROGRAM").as_deref(), Some("kitty"));
    assert_eq!(
        env_of(&cmd, "FELIS_TERM"),
        None,
        "the hatch configures the stamp; it is not part of it"
    );

    if std::env::var_os("FELIS_TERM").is_none() {
        let bare = command_from_args(&args, &factory, 0, &resolved_request(&[]), None);
        assert_eq!(env_of(&bare, "TERM").as_deref(), Some(crate::DEFAULT_TERM));
    }
}

#[cfg(unix)]
fn agent_link_in(dir: &TempDir) -> Arc<crate::agent::AgentLink> {
    Arc::new(
        crate::agent::AgentLink::for_endpoint(&Endpoint::unix(dir.path().join("daemon.sock")))
            .unwrap()
            .unwrap(),
    )
}

/// Only a relayed base has `SSH_AUTH_SOCK` rewritten: a forwarded socket
/// dies with its SSH link, and a child's environment cannot change after
/// exec.
#[cfg(unix)]
#[test]
fn only_a_relay_base_is_repointed_at_the_stable_agent_path() {
    let dir = private_dir();
    let link = agent_link_in(&dir);
    let factory: SessionFactory = Arc::new(|_| Command::new("/bin/sh"));
    let args = felis_protocol::messages::SpawnArgs::default();
    let forwarded = vec![entry("SSH_AUTH_SOCK", "/tmp/ssh-abc/agent.42")];

    let relayed = spawn_command_for_test(
        &args,
        &factory,
        EnvBaseSource::Relay(&forwarded),
        Some(&link),
        None,
    );
    assert_eq!(
        env_of(&relayed, "SSH_AUTH_SOCK").as_deref(),
        link.path().to_str(),
        "a relay base points the child at the daemon's stable link"
    );

    let requested = spawn_command_for_test(
        &args,
        &factory,
        EnvBaseSource::Request(&forwarded),
        Some(&link),
        None,
    );
    assert_eq!(
        env_of(&requested, "SSH_AUTH_SOCK").as_deref(),
        Some("/tmp/ssh-abc/agent.42"),
        "a local dial's own agent path is left alone"
    );
}

#[test]
fn a_snapshot_or_relay_base_loses_its_stale_origin_variables() {
    let factory: SessionFactory =
        Arc::new(|_| panic!("a named command must not reach the default-program factory"));
    let base: Vec<_> = crate::ORIGIN_ENV
        .iter()
        .map(|key| entry(key, "stale"))
        .collect();
    let current: Vec<_> = crate::ORIGIN_ENV
        .iter()
        .map(|key| ((*key).to_owned(), format!("current-{key}")))
        .collect();
    for source in [EnvBaseSource::Request(&base), EnvBaseSource::Relay(&base)] {
        let resolved =
            resolve_env_base(source, None, None, crate::child_env::TARGET_IS_WINDOWS).unwrap();
        for explicit in [Vec::new(), current.clone()] {
            let args = felis_protocol::messages::SpawnArgs {
                command: "printenv".to_owned(),
                env: explicit.clone(),
                ..Default::default()
            };
            let cmd = command_from_args(&args, &factory, 0, &resolved, None);
            for key in crate::ORIGIN_ENV {
                let want = explicit
                    .iter()
                    .find(|(name, _)| name == key)
                    .map(|(_, value)| value.clone());
                assert_eq!(env_of(&cmd, key), want, "{key} from {source:?}");
            }
        }
    }
}

/// The link may only name what a child could have inherited: the same
/// sanitization a create runs decides the target.
#[test]
fn the_agent_target_is_derived_from_the_sanitized_block() {
    let forwarded = |env| forwarded_agent_socket(&CarrierBlock { env });

    assert_eq!(
        forwarded(vec![entry("SSH_AUTH_SOCK", "/tmp/ssh-abc/agent.42")]),
        Some(PathBuf::from("/tmp/ssh-abc/agent.42"))
    );

    assert_eq!(
        forwarded(vec![
            entry("BAD=NAME", "x"),
            entry("SSH_AUTH_SOCK", "/tmp/ssh-abc/agent.42"),
        ]),
        None
    );
}

/// A relay that forwarded no agent describes a session that never had one.
#[cfg(unix)]
#[test]
fn a_relay_base_without_an_agent_gains_none() {
    let dir = private_dir();
    let link = agent_link_in(&dir);
    let factory: SessionFactory = Arc::new(|_| Command::new("/bin/sh"));
    let args = felis_protocol::messages::SpawnArgs::default();
    let base = vec![entry("PATH", "/bin")];
    let cmd = spawn_command_for_test(
        &args,
        &factory,
        EnvBaseSource::Relay(&base),
        Some(&link),
        None,
    );
    assert_eq!(env_of(&cmd, "SSH_AUTH_SOCK"), None);
}

fn env_of(cmd: &Command, key: &str) -> Option<String> {
    let wanted = if cfg!(windows) {
        key.to_ascii_uppercase()
    } else {
        key.to_owned()
    };
    cmd.get_envs()
        .find(|(k, _)| *k == OsStr::new(&wanted))
        .map(|(_, v)| v.to_string_lossy().into_owned())
}

#[cfg(unix)]
fn spawn_command_for_test(
    args: &felis_protocol::messages::SpawnArgs,
    factory: &SessionFactory,
    base: EnvBaseSource<'_>,
    agent: Option<&Arc<crate::agent::AgentLink>>,
    endpoint: Option<&OsStr>,
) -> Command {
    let resolved = resolve_env_base(
        base,
        agent.map(AsRef::as_ref),
        endpoint,
        crate::child_env::TARGET_IS_WINDOWS,
    )
    .unwrap();
    command_from_args(args, factory, 0, &resolved, None)
}

/// The requested cwd is where the default program starts.
#[cfg(unix)]
#[test]
fn spawn_with_args_starts_the_default_program_in_the_requested_cwd() {
    let tmp = private_dir();
    let dir = tmp.path().canonicalize().unwrap();
    let out = dir.join("pwd");
    let script = format!("pwd -P > '{}'", out.display());
    let factory: SessionFactory = Arc::new(move |_| {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", &script]);
        cmd.env_clear();
        cmd
    });
    let args = felis_protocol::messages::SpawnArgs {
        cwd: dir.to_str().unwrap().to_owned(),
        ..Default::default()
    };
    // `Session` is not `Debug`, so match rather than `expect`.
    let _spawned = match spawn_with_args(
        &args,
        &factory,
        0,
        EnvBaseSource::Birth,
        None,
        None,
        None,
        crate::DEFAULT_SIZE,
    ) {
        Ok(spawned) => spawned,
        Err(err) => panic!("spawn in an existing cwd: {err}"),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let written = loop {
        if let Ok(text) = std::fs::read_to_string(&out)
            && text.ends_with('\n')
        {
            break text;
        }
        assert!(Instant::now() < deadline, "the child never wrote its cwd");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(written.trim_end(), dir.to_str().unwrap());
}

/// A relative `cwd` is refused rather than resolved against the daemon's
/// frozen directory.
#[test]
fn spawn_with_args_refuses_a_relative_cwd() {
    let factory: SessionFactory =
        Arc::new(|_| panic!("the default factory must not run for an invalid SpawnArgs"));
    let args = felis_protocol::messages::SpawnArgs {
        cwd: "build".to_owned(),
        ..Default::default()
    };
    match spawn_with_args(
        &args,
        &factory,
        0,
        EnvBaseSource::Birth,
        None,
        None,
        None,
        crate::DEFAULT_SIZE,
    ) {
        Ok(_) => panic!("a relative cwd must be rejected"),
        Err(err) => assert!(
            err.to_string().contains("cwd must be absolute"),
            "the refusal reason must name the problem, got: {err}"
        ),
    }
}

/// An unusable cwd retries in the daemon's own for a bare window launch; a
/// named command keeps failing, since the directory is part of what was
/// asked.
#[cfg(unix)]
#[test]
fn spawn_with_args_falls_back_to_the_daemon_cwd_only_for_a_bare_launch() {
    let daemon_cwd = std::env::current_dir()
        .unwrap()
        .into_os_string()
        .into_string()
        .unwrap();
    let factory: SessionFactory = Arc::new(|_| sh("exit 0", "/bin:/usr/bin"));
    let missing = "/felis-no-such-directory-for-tests";

    let bare = felis_protocol::messages::SpawnArgs {
        cwd: missing.to_owned(),
        ..Default::default()
    };
    // `Session` is not `Debug`, so match rather than `expect`.
    match spawn_with_args(
        &bare,
        &factory,
        0,
        EnvBaseSource::Birth,
        None,
        None,
        None,
        crate::DEFAULT_SIZE,
    ) {
        Ok(spawned) => assert_eq!(
            spawned.cwd.as_deref(),
            Some(daemon_cwd.as_str()),
            "the fallback reports the directory the shell started in, not the one asked for"
        ),
        Err(err) => panic!("a bare launch must survive an unusable cwd, got: {err}"),
    }

    let named = felis_protocol::messages::SpawnArgs {
        command: "/bin/sh".to_owned(),
        args: vec!["-c".to_owned(), "exit 0".to_owned()],
        cwd: missing.to_owned(),
        ..Default::default()
    };
    assert!(
        spawn_with_args(
            &named,
            &factory,
            0,
            EnvBaseSource::Birth,
            None,
            None,
            None,
            crate::DEFAULT_SIZE
        )
        .is_err(),
        "a named command must not be run somewhere other than its requested cwd",
    );
}

/// An empty `cwd` asks for the daemon's own, which is what the child
/// inherits.
#[cfg(unix)]
#[test]
fn spawn_with_args_reports_the_daemon_cwd_for_an_empty_one() {
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "exit 0"]);
        cmd
    });
    let daemon_cwd = std::env::current_dir()
        .unwrap()
        .into_os_string()
        .into_string()
        .unwrap();
    match spawn_with_args(
        &felis_protocol::messages::SpawnArgs::default(),
        &factory,
        0,
        EnvBaseSource::Birth,
        None,
        None,
        None,
        crate::DEFAULT_SIZE,
    ) {
        Ok(spawned) => assert_eq!(spawned.cwd, Some(daemon_cwd)),
        Err(err) => panic!("a bare launch spawns, got: {err}"),
    }
}

/// Each `SpawnArgs` field falls back independently (docs/reference/ipc.md),
/// so `env` reaches the default program too.
#[test]
fn command_from_args_applies_env_overrides_to_the_default_program() {
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = Command::new("/bin/sh");
        cmd.env_clear();
        cmd
    });

    let args = felis_protocol::messages::SpawnArgs {
        env: vec![("FELIS_ENV_PROBE".to_owned(), "reached".to_owned())],
        ..Default::default()
    };
    let cmd = command_from_args(&args, &factory, 0, &ResolvedEnv::birth(), None);
    let probe = cmd
        .get_envs()
        .find(|(k, _)| *k == OsStr::new("FELIS_ENV_PROBE"))
        .map(|(_, v)| v.to_owned());
    assert_eq!(probe, Some(OsString::from("reached")));
}

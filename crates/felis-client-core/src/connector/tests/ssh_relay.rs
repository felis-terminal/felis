use super::*;

#[test]
fn relay_command_builds_the_ssh_stdio_relay_invocation() {
    let cmd = relay_command("user@host", &[], RemoteSpawn::Allow);
    let std_cmd = cmd.as_std();
    assert_eq!(std_cmd.get_program(), "ssh");
    let args: Vec<&std::ffi::OsStr> = std_cmd.get_args().collect();
    assert_eq!(args, ["--", "user@host", "felis-daemon", "relay"]);
}

#[test]
fn relay_command_appends_no_spawn_for_refused_remote_spawn() {
    let cmd = relay_command("user@host", &[], RemoteSpawn::Refuse);
    let args: Vec<&std::ffi::OsStr> = cmd.as_std().get_args().collect();
    assert_eq!(
        args,
        ["--", "user@host", "felis-daemon", "relay", "--no-spawn"]
    );
}

/// The landing half of the auto-spawn matrix: a window launch and
/// a retarget's landing both mean "hold this session for me", so
/// their relay command must leave the remote daemon free to start
/// (docs/reference/cli.md "Auto-spawning").
#[test]
fn the_landing_relay_command_lets_the_remote_daemon_spawn() {
    let cmd = relay_command("user@host", &[], crate::LANDING_REMOTE_SPAWN);
    let args: Vec<&std::ffi::OsStr> = cmd.as_std().get_args().collect();
    assert_eq!(args, ["--", "user@host", "felis-daemon", "relay"]);
}

/// ssh requires its options before the destination.
#[test]
fn relay_command_splices_ssh_args_before_the_destination() {
    let ssh_args = vec!["-p".to_string(), "2222".to_string(), "-i".to_string()];
    let cmd = relay_command("user@host", &ssh_args, RemoteSpawn::Allow);
    let std_cmd = cmd.as_std();
    let args: Vec<&std::ffi::OsStr> = std_cmd.get_args().collect();
    assert_eq!(
        args,
        [
            "-p",
            "2222",
            "-i",
            "--",
            "user@host",
            "felis-daemon",
            "relay"
        ],
    );
}

/// A destination is data: one spelled like an option still reaches ssh
/// after the terminator, whatever the caller's own arguments end with.
#[test]
fn an_option_like_destination_lands_after_the_terminator() {
    for ssh_args in [
        vec![],
        vec!["--".to_string()],
        vec!["-i".to_string(), "--".to_string()],
    ] {
        let cmd = relay_command("-oProxyCommand=false", &ssh_args, RemoteSpawn::Allow);
        let args: Vec<&std::ffi::OsStr> = cmd.as_std().get_args().collect();
        let tail = &args[ssh_args.len()..];
        assert_eq!(
            tail,
            ["--", "-oProxyCommand=false", "felis-daemon", "relay"],
            "{ssh_args:?}"
        );
    }
}

/// The spawn and the handshake as one step, which is what a caller
/// without its own deadline does with them.
async fn connect_stdio_command(
    cmd: Command,
    offer: Offer,
) -> Result<CarrierConnection, ConnectError> {
    connect_relay_armed(cmd, offer).await
}

/// A child that will not start is not the endpoint answering:
/// `ssh` missing from `PATH` raises `NotFound` like a cold socket
/// does, and a daemon started here would serve nobody.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_stdio_command_returns_io_error_when_command_missing() {
    let cmd = Command::new("/this/binary/does/not/exist");
    let result = connect_stdio_command(cmd, Offer::ops()).await;
    let Err(err) = result else {
        panic!("a command that does not exist cannot connect");
    };
    let ConnectError::Io(io_err) = &err else {
        panic!("expected an Io error, got: {err}");
    };
    assert_eq!(io_err.kind(), std::io::ErrorKind::NotFound);
    assert!(
        !err.may_be_a_cold_socket(),
        "a child that never started says nothing about the endpoint"
    );
}

/// A child that exits without writing surfaces as a transient
/// preface IO failure, the shape of an ssh connect failure.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_stdio_command_fails_the_preface_transiently_when_the_child_exits_silently() {
    let cmd = Command::new("true");
    let result = connect_stdio_command(cmd, Offer::ops()).await;
    assert!(matches!(
        result,
        Err(ConnectError::Preface(PrefaceExchangeError::Io(_)))
    ));
    assert!(result.err().is_some_and(|e| e.is_transient()));
}

/// A relay that never answers stands in for `ssh` stuck connecting,
/// which reads no stdin, so only a kill can end it.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abandoned_ssh_dial_kills_its_relay_child() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("pid");
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "echo $$ > \"$1\"; exec sleep 60", "relay"])
        .arg(&marker);
    let dial = tokio::spawn(connect_relay_armed(cmd, Offer::ops()));
    let deadline = Instant::now() + Duration::from_secs(5);
    let pid = loop {
        if let Ok(pid) = std::fs::read_to_string(&marker)
            && pid.ends_with('\n')
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "the relay child never started");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    dial.abort();
    assert!(matches!(dial.await, Err(err) if err.is_cancelled()));
    loop {
        // The shell builtin: the Nix build sandbox on Linux has no /bin/kill.
        let alive = std::process::Command::new("/bin/sh")
            .args(["-c", "kill -0 \"$1\"", "kill", pid.trim()])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        if !alive {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the relay child survived the abandoned dial"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

use super::*;

/// A daemon that completes the handshake and then answers the roster
/// query with a window-only push, an arm an `Ops` link may never
/// receive (REQ-114). It is the protocol class of the failure matrix
/// below: the peer answered, but not in this protocol.
async fn rogue_daemon(listener: felis_transport::Listener) {
    use felis_protocol::MessageKind;
    use felis_protocol::messages::{ConnToClientMsg, PushMsg};
    use felis_protocol::preface::{DaemonPreface, PROTOCOL_MAJOR, PROTOCOL_MINOR};
    use felis_transport::{
        FrameReader, FrameWriter,
        preface::{read_client_bootstrap, write_daemon_preface},
        server_split,
    };

    let stream = listener.accept().await.expect("accept the CLI");
    let (mut read_half, mut write_half) = server_split(stream);
    let _bootstrap = read_client_bootstrap(&mut read_half)
        .await
        .expect("client preface");
    write_daemon_preface(
        &mut write_half,
        DaemonPreface::Accept {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
        },
    )
    .await
    .expect("daemon preface");

    let mut reader = FrameReader::new(read_half);
    let mut writer = FrameWriter::at_build_minor(write_half);
    let hello = reader
        .next_frame()
        .await
        .expect("read Hello")
        .expect("the CLI sends Hello");
    assert_eq!(hello.kind, MessageKind::Conn.as_u16());
    writer
        .send(&ConnToClientMsg::Welcome { identity: None })
        .await
        .expect("write Welcome");
    let _request = reader
        .next_frame()
        .await
        .expect("read the roster query")
        .expect("the CLI queries the roster");
    writer
        .send(&PushMsg::Reattach { id: 1 })
        .await
        .expect("write the rogue push");
    // Holding the socket open keeps a hang distinguishable from the
    // refusal the verb owes.
    tokio::time::sleep(Duration::from_secs(30)).await;
}

/// A failure the verb meets before its body still wears the verb's
/// framing: one error object on stderr, nothing on stdout, and the
/// kind's exit code (docs/reference/cli.md "Machine output").
#[test]
fn a_failure_before_the_verb_body_is_framed_as_the_verb_would() {
    let tmp = private_dir();

    // Resolving a relative `--config` reads the current directory,
    // which a deleted one denies.
    let gone = tmp.path().join("gone");
    std::fs::create_dir(&gone).unwrap();
    let script = format!(
        "cd {dir} && rmdir {dir} && exec {felis} --config felis.toml config path --format json",
        dir = gone.display(),
        felis = cli_bin().display(),
    );
    let unresolvable = StdCommand::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        .env_remove("FELIS_SOCKET")
        .env_remove("RUST_LOG")
        .output()
        .expect("run felis with an unresolvable --config");
    assert_framed_failure(&unresolvable, "usage", 2);

    // A directory as stdin: `-` asks for a payload the process cannot
    // read, which is its own input channel failing rather than an
    // empty payload.
    let sink = std::fs::File::open(tmp.path()).unwrap();
    let unreadable = cli_on(tmp.path().join("daemon.sock"))
        .args(["sessions", "send", "deadbeef", "-", "--format", "json"])
        .stdin(std::process::Stdio::from(sink))
        .output()
        .expect("run felis sessions send - with an unreadable stdin");
    assert_framed_failure(&unreadable, "input_failed", 2);
}

/// The machine contract's failure classes, one per row: what the exit
/// code is, and that stderr carries exactly one parseable object. The
/// exception is a refusal clap states, which precedes any framing and
/// stays human (docs/reference/cli.md "Machine output").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_machine_failure_class_is_one_object_and_one_code() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory());
    let cold = tmp.path().join("no-daemon.sock");
    let rogue = tmp.path().join("rogue.sock");
    let rogue_task = tokio::spawn(rogue_daemon(bind_listener(&rogue)));
    let socket_arg = socket.display().to_string();
    let cold_arg = cold.display().to_string();
    let rogue_arg = rogue.display().to_string();

    // A refusal clap can state: human, exit 2, no framing chosen yet.
    let clap_usage = cli_command()
        .args([
            "--socket",
            &socket_arg,
            "sessions",
            "send",
            "--format",
            "json",
            "deadbeef",
        ])
        .output()
        .expect("run felis sessions send with no payload");
    assert_eq!(clap_usage.status.code(), Some(2));
    assert_eq!(clap_usage.stdout, Vec::<u8>::new());
    assert!(
        serde_json::from_str::<serde_json::Value>(
            String::from_utf8_lossy(&clap_usage.stderr).trim()
        )
        .is_err(),
        "a clap error is a human message: {}",
        String::from_utf8_lossy(&clap_usage.stderr),
    );

    for (argv, code, kind) in [
        // Usage after a successful parse: the verb's own framing.
        (
            vec![
                "--socket",
                &socket_arg,
                "window",
                "retarget",
                "--format",
                "json",
            ],
            2,
            "usage",
        ),
        // A typed refusal the daemon answered on the merits.
        (
            vec![
                "--socket",
                &socket_arg,
                "sessions",
                "spawn",
                "--rows",
                "9999",
                "--cols",
                "80",
                "--format",
                "json",
            ],
            1,
            "invalid_request",
        ),
        // Transport: nothing answered at the endpoint.
        (
            vec![
                "--socket", &cold_arg, "sessions", "info", "1a", "--format", "json",
            ],
            2,
            "daemon_unreachable",
        ),
        // Protocol: the peer answered, but not in this protocol.
        (
            vec![
                "--socket", &rogue_arg, "sessions", "info", "1a", "--format", "json",
            ],
            2,
            "protocol",
        ),
    ] {
        let out = cli_command().args(&argv).output().expect("run the verb");
        assert_eq!(
            out.status.code(),
            Some(code),
            "{argv:?}: stderr={}",
            String::from_utf8_lossy(&out.stderr),
        );
        assert!(
            out.stdout.is_empty(),
            "{argv:?} wrote to stdout: {}",
            String::from_utf8_lossy(&out.stdout),
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 1, "{argv:?} stderr is one object: {stderr}");
        let object = parse_point(lines[0], "the failure object");
        assert_eq!(object["error"]["kind"], kind, "{argv:?}: {stderr}");
        assert!(object["error"]["message"].is_string(), "{argv:?}: {stderr}");
    }

    rogue_task.abort();
    server.abort();
}

/// A machine format silences the console log: with `felis-daemon`
/// unreachable the autospawn path logs at INFO, which would make a
/// point verb's one error object the second line on stderr.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn machine_mode_keeps_the_log_off_the_error_channel() {
    let tmp = private_dir();
    // Copied out of the cargo target directory so the sibling lookup
    // finds no `felis-daemon`, and run with an empty PATH so the spawn
    // fails at once instead of waiting on a real daemon.
    let bin = tmp.path().join("felis");
    std::fs::copy(cli_bin(), &bin).unwrap();
    let socket = tmp.path().join("no-daemon.sock");

    let out = felis_command(&bin)
        .env("PATH", "")
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "spawn", "--format", "json"])
        .output()
        .expect("run felis sessions spawn --format json");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "one object, no log line: {stderr}");
    let object = parse_point(lines[0], "the failure object");
    assert_eq!(object["error"]["kind"], "daemon_unreachable", "{stderr}");
}

/// Human output keeps the autospawn's INFO and DEBUG progress lines off
/// stderr by default, leaving only the failure itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn human_mode_logs_nothing_below_warn_by_default() {
    let tmp = private_dir();
    let bin = tmp.path().join("felis");
    std::fs::copy(cli_bin(), &bin).unwrap();
    let socket = tmp.path().join("no-daemon.sock");

    let out = felis_command(&bin)
        .env("PATH", "")
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "spawn"])
        .output()
        .expect("run felis sessions spawn");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.is_empty(), "the failure is still reported");
    assert!(
        !stderr.contains("INFO") && !stderr.contains("DEBUG"),
        "no progress log lines: {stderr}"
    );
}

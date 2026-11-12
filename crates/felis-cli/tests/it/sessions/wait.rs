use super::*;

/// The printed `1` is `false`'s live D mark; the connection subscribes
/// before the input, so even an instant command cannot slip past.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_wait_prints_the_commands_exit_code() {
    let tmp = private_dir();
    let factory = sh_factory(
        "printf '\\033]133;A\\007$ '; read x; eval \"$x\"; \
             printf '\\033]133;D;%d\\007' \"$?\"; read _y",
    );
    let (server, socket, _pool) = spawn_daemon(&tmp, factory);
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let out = felis_out(
        &socket,
        &[
            "sessions",
            "send",
            &id_hex,
            "false",
            "--key",
            "enter",
            "--wait",
            "--timeout",
            "30",
        ],
    );
    assert!(
        out.status.success(),
        "send --wait must exit 0 on an observed D mark: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "1",
        "stdout must be `false`'s exit code",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_wait_json_emits_the_wait_shape() {
    let tmp = private_dir();
    let factory = sh_factory(
        "printf '\\033]133;A\\007$ '; read x; eval \"$x\"; \
             printf '\\033]133;D;%d\\007' \"$?\"; read _y",
    );
    let (server, socket, _pool) = spawn_daemon(&tmp, factory);
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let out = felis_out(
        &socket,
        &[
            "sessions",
            "send",
            &id_hex,
            "false",
            "--key",
            "enter",
            "--wait",
            "--timeout",
            "30",
            "--format",
            "json",
        ],
    );
    assert!(
        out.status.success(),
        "send --wait --format json must exit 0: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let waited = parse_point(
        &String::from_utf8_lossy(&out.stdout),
        "sessions send --wait",
    );
    assert_eq!(waited["id"], id_hex.as_str());
    assert_eq!(
        waited["exit_code"], 1,
        "stdout must carry the awaited mark's code",
    );

    server.abort();
}

/// The rehydrate burst replays historical marks; a wait that adopted
/// one would return the previous command's code on every idle session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn payload_less_wait_ignores_historical_marks_and_times_out() {
    let tmp = private_dir();
    let factory = sh_factory("printf '\\033]133;D;7\\007'; read x");
    let (server, socket, _pool) = spawn_daemon(&tmp, factory);
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    poll_until(
        &socket,
        &["sessions", "info", &id_hex, "--format", "json"],
        "\"last_exit_code\":7",
    )
    .await;

    let out = felis_out(
        &socket,
        &["sessions", "send", &id_hex, "--wait", "--timeout", "1"],
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "wait must time out (exit 1), not adopt the historical D;7: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no command completed"),
        "stderr must name the timeout",
    );

    server.abort();
}

/// A wait with no `--timeout` cannot end on the clock, so a session
/// that exits with no `D` mark must end it, with or without input sent
/// ahead of the watch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wait_without_a_timeout_ends_session_ended_when_the_session_exits_markless() {
    for with_input in [false, true] {
        let tmp = private_dir();
        let factory = sh_factory("read x; exit 0");
        let (server, socket, _pool) = spawn_daemon(&tmp, factory);
        let id = create_and_detach(&socket).await;
        let id_hex = format!("{id:032x}");

        let mut args = vec!["sessions", "send", &id_hex];
        if with_input {
            args.extend(["--key", "enter"]);
        }
        args.extend(["--wait", "--format", "json"]);
        let waiter = cli_on(&socket)
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("run felis sessions send --wait");
        if !with_input {
            // Only a watch already registered can be ended by the exit.
            tokio::time::sleep(Duration::from_millis(300)).await;
            let out = felis_out(&socket, &["sessions", "send", &id_hex, "--key", "enter"]);
            assert!(out.status.success(), "the line that ends the shell is sent");
        }
        let out = tokio::task::spawn_blocking(move || waiter.wait_with_output())
            .await
            .unwrap()
            .expect("wait for felis sessions send --wait");
        assert_eq!(
            out.status.code(),
            Some(1),
            "with_input={with_input}: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("\"kind\":\"session_ended\""),
            "with_input={with_input}: {}",
            String::from_utf8_lossy(&out.stderr),
        );
        server.abort();
    }
}

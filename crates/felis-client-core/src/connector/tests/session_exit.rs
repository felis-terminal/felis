use super::*;

#[cfg(unix)]
async fn await_session_exited(
    conn: &mut Connection,
    deadline: tokio::time::Instant,
) -> Option<(u128, Option<felis_protocol::messages::ExitStatus>)> {
    use felis_protocol::messages::PushMsg;
    while tokio::time::Instant::now() < deadline {
        // A quiet stretch is not an answer: the deadline above is the
        // one bound, and a loaded machine running the whole suite in
        // parallel can leave the socket silent for seconds before the
        // exit propagates.
        let frame = match tokio::time::timeout(Duration::from_secs(3), conn.next_frame()).await {
            Ok(Ok(Some(frame))) => frame,
            Err(_elapsed) => continue,
            _ => return None,
        };
        if frame.kind != MessageKind::Push.as_u16() {
            continue;
        }
        if let Ok(PushMsg::SessionExited { id, status }) = codec::decode::<PushMsg>(&frame.body) {
            return Some((id, status));
        }
    }
    None
}

#[cfg(all(unix, feature = "daemon-tests"))]
async fn daemon_with_exiting_shell() -> (TempDir, PathBuf, tokio::task::JoinHandle<()>) {
    daemon_running("printf bye; exit 0").await
}

/// The daemon pushes `SessionExited` before dropping the
/// subscription (session-lifecycle.md "Telling the attached window
/// the shell exited").
#[cfg(all(unix, feature = "daemon-tests"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn window_client_hears_session_exited_when_shell_exits() {
    let (_tmp, path, server) = daemon_with_exiting_shell().await;

    let mut c = connect(&path, Offer::window(false)).await.unwrap();
    let attach = c.create_with(SpawnArgs::default()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    assert_eq!(
        await_session_exited(&mut c, deadline)
            .await
            .map(|(id, _)| id),
        Some(attach.id),
        "a window client must hear SessionExited for its own session on shell exit",
    );

    server.abort();
}

#[cfg(all(unix, feature = "daemon-tests"))]
async fn session_exit_status_of(
    script: &'static str,
) -> Option<felis_protocol::messages::ExitStatus> {
    let (_tmp, path, server) = daemon_running(script).await;
    let mut c = connect(&path, Offer::window(false)).await.unwrap();
    c.create_with(SpawnArgs::default()).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let exited = await_session_exited(&mut c, deadline).await;
    server.abort();
    exited.expect("the window must hear SessionExited").1
}

#[cfg(all(unix, feature = "daemon-tests"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_exited_carries_the_programs_exit_code() {
    assert_eq!(
        session_exit_status_of("exit 3").await,
        Some(felis_protocol::messages::ExitStatus::Code(3)),
    );
}

#[cfg(all(unix, feature = "daemon-tests"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_exited_carries_the_signal_that_killed_the_program() {
    assert_eq!(
        session_exit_status_of("kill -KILL $$").await,
        Some(felis_protocol::messages::ExitStatus::Signal(9)),
    );
}

// macOS keeps a session leader's controlling terminal open until the
// leader exits, so there the PTY never closes under a running program.
#[cfg(all(unix, not(target_os = "macos"), feature = "daemon-tests"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_exited_carries_no_status_when_the_program_outlives_its_pty() {
    assert_eq!(
        // The fixture's PATH has no `sleep` on every host, so a bounded
        // builtin loop keeps the shell alive past the daemon's reap wait.
        session_exit_status_of(
            "exec </dev/null >/dev/null 2>&1; i=0; while [ \"$i\" -lt 3000000 ]; do i=$((i+1)); done"
        )
        .await,
        None,
    );
}

// The macOS side of the split above: the PTY closes only when the program
// exits, so the window hears its real status.
#[cfg(all(target_os = "macos", feature = "daemon-tests"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_exited_carries_the_status_of_a_program_that_closed_its_pty() {
    assert_eq!(
        session_exit_status_of(
            "exec </dev/null >/dev/null 2>&1; i=0; while [ \"$i\" -lt 20000 ]; do i=$((i+1)); done; exit 7"
        )
        .await,
        Some(felis_protocol::messages::ExitStatus::Code(7)),
    );
}

#[cfg(all(unix, feature = "daemon-tests"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ops_attach_gets_channel_close_without_session_exited() {
    use felis_protocol::messages::PushMsg;

    let (_tmp, path, server) = daemon_with_exiting_shell().await;

    let mut c = connect(&path, Offer::ops()).await.unwrap();
    c.create_with(SpawnArgs::default()).await.unwrap();

    let mut saw_exited = false;
    let mut closed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(3), c.reader.next_frame()).await {
            Ok(Ok(Some(frame))) => {
                if frame.kind == MessageKind::Push.as_u16()
                    && matches!(
                        codec::decode::<PushMsg>(&frame.body),
                        Ok(PushMsg::SessionExited { .. })
                    )
                {
                    saw_exited = true;
                    break;
                }
            }
            Ok(Ok(None)) => {
                closed = true;
                break;
            }
            // As above: silence is not the close this test waits for,
            // so the loop simply goes round again.
            Err(_elapsed) => {}
            _ => break,
        }
    }
    assert!(
        !saw_exited,
        "a scripted Ops attach must not receive SessionExited",
    );
    assert!(
        closed,
        "the daemon must close the subscription on shell exit so the window re-dials",
    );

    server.abort();
}

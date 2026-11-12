use super::*;

#[tokio::test]
async fn a_fence_after_the_program_exited_reports_the_session_ended() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (conn, mut rx) = requester(&mut task);
    let watch = Some((conn, test_stream(1)));
    assert_eq!(task.prepare_send(watch, &[]), Prepared::Ready);
    assert_eq!(task.prepare_send(None, &[]), Prepared::Ready);

    task.pty_eof = true;
    task.end_watches();
    assert!(!task.send_fence(None, 1), "a plain send answers ended");
    assert!(
        !task.send_fence(watch, 1),
        "the watch's stream already ended"
    );
    let events = drain_events(&mut rx);
    assert!(
        matches!(
            events.as_slice(),
            [OutEvent::Control(ConnToClientMsg::Error {
                subject: Subject::Stream(stream),
                reason: StreamErrorReason::Unavailable,
                ..
            })] if *stream == test_stream(1)
        ),
        "the watch ends once, with no WaitSent: {events:?}"
    );
    assert_eq!(task.prepare_send(watch, &[]), Prepared::Ended);
}

#[tokio::test]
async fn a_child_exit_before_eof_ends_waits_and_refuses_the_fence() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (conn, mut rx) = requester(&mut task);
    let released = Some((conn, test_stream(1)));
    let pending = Some((conn, test_stream(2)));
    assert_eq!(task.prepare_send(released, &[]), Prepared::Ready);
    assert!(task.send_fence(released, 1));
    assert_eq!(task.prepare_send(pending, &[]), Prepared::Ready);
    drop(drain_events(&mut rx));

    task.on_child_exit(None);
    assert!(!task.pty_eof, "the PTY is still open");
    assert!(!task.send_fence(None, 1), "a plain send answers ended");
    assert!(!task.send_fence(pending, 1), "the pending wait ended");
    let ended: Vec<_> = drain_events(&mut rx)
        .into_iter()
        .map(|ev| match ev {
            OutEvent::Control(ConnToClientMsg::Error {
                subject: Subject::Stream(stream),
                reason: StreamErrorReason::Unavailable,
                ..
            }) => stream,
            other => panic!("expected a wait to end Unavailable, got {other:?}"),
        })
        .collect();
    assert_eq!(ended, [test_stream(1), test_stream(2)]);
    assert_eq!(task.prepare_send(None, &[]), Prepared::Ended);
}

/// The exit poll can reap a child whose last output the sink has not
/// parsed yet: the parked pacer holds back the 2 MiB ahead of the mark.
/// The background `read` keeps the PTY open past the exit, so no EOF
/// comes to order the two.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reaped_child_ends_its_wait_with_the_mark_it_wrote_last() {
    for holder in ["", "( read -r _hold <&1 ) &"] {
        let mut task = SessionTask::for_tests(session_with(&format!(
            "read -r _go; {holder} s=xxxxxxxxxxxxxxxx; \
                 for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17; do s=$s$s; done; \
                 printf '%s\\033]133;D;4\\007' \"$s\"; exit 4"
        )));
        let (conn, mut rx) = requester(&mut task);
        let watch = Some((conn, test_stream(1)));
        assert_eq!(task.prepare_send(watch, &[]), Prepared::Ready);
        assert!(task.send_fence(watch, 1));
        drop(drain_events(&mut rx));

        streaming::write_pty_reserved(&task.session.writer, b"go\n".to_vec(), None).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while !task.child_exited {
            assert!(
                tokio::time::Instant::now() < deadline,
                "holder={holder:?}: the exit never landed"
            );
            task.poll_child_exit();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let events = drain_events(&mut rx);
        assert!(
            matches!(
                events.first(),
                Some(OutEvent::Ops { msg, .. })
                    if matches!(**msg, OpsToClientMsg::Mark { exit_code: Some(4) })
            ),
            "holder={holder:?}: the mark ends the wait, not the exit: {events:?}"
        );
        assert_eq!(task.prepare_send(None, &[]), Prepared::Ended);
        let handover =
            felis_pty::handover::Handover::of(&task.session.resizer, &task.session.child).unwrap();
        if let Some(pid) = rustix::process::Pid::from_raw(handover.pid()) {
            rustix::process::kill_process_group(pid, rustix::process::Signal::HUP)
                .unwrap_or_default();
        }
    }
}

#[tokio::test]
async fn a_dropped_requester_leaves_no_stream_running() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (conn, _rx) = requester(&mut task);
    assert_eq!(
        task.prepare_send(Some((conn, test_stream(1))), &[]),
        Prepared::Ready
    );
    task.start_rows(
        Owner::Conn(conn),
        RegionSource::Visible,
        false,
        None,
        test_stream(2),
    );
    assert!(task.has_pending_chunks());
    task.handle_cmd(SessionCmd::DropRequester { id: conn });
    assert!(task.requesters.is_empty());
    assert!(!task.has_pending_chunks());
}

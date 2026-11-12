use super::*;

fn fence_replies(events: &[OutEvent]) -> Vec<Correlation> {
    events
        .iter()
        .filter_map(|ev| match ev {
            OutEvent::Session {
                msg: SessionToClientMsg::InputAccepted,
                correlation,
            } => Some(*correlation),
            _ => None,
        })
        .collect()
}

/// The reply is the requester's alone, and carries the id of the
/// fence it answers: a caller with two barriers outstanding pairs
/// them by nothing else.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fence_answers_the_subscriber_that_asked_on_its_own_request_id() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (sub_a, mut rx_a) = attach_sub(&mut task, false);
    let (_sub_b, mut rx_b) = attach_sub(&mut task, false);

    assert!(fence(&mut task, sub_a).is_none());

    assert_eq!(
        fence_replies(&drain_events(&mut rx_a)),
        vec![Correlation::request(test_request())],
    );
    assert!(
        fence_replies(&drain_events(&mut rx_b)).is_empty(),
        "a barrier is the asking connection's own, never a broadcast",
    );
}

/// A resize generates its in-band report only under DECSET 2048, so
/// on a grid that asked for none the fence behind it answers with
/// nothing written to the child.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resize_before_a_fence_writes_nothing_and_still_answers() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (sub, mut rx) = attach_sub(&mut task, false);

    assert!(
        task.handle_input(
            sub,
            InputMsg::Resize {
                dims: RequestedDims {
                    rows: 30,
                    cols: 100,
                    pixel_w: 0,
                    pixel_h: 0,
                },
            },
        )
        .is_none()
    );
    assert!(fence(&mut task, sub).is_none());

    assert_eq!(fence_replies(&drain_events(&mut rx)).len(), 1);
    assert!(
        !task.pty_steps.contains(&PtyStep::ResizeReport),
        "a grid that asked for no resize report gets none: {:?}",
        task.pty_steps,
    );
}

/// Order, not delivery: the report a focus change generates is
/// attempted before the fence is answered, and on a writer with
/// room it reaches the PTY. The reply still says only that the
/// input was processed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_focus_report_is_written_before_the_fence_behind_it_answers() {
    let mut task = SessionTask::for_tests(session_with(
        "printf '\\033[?1004h'; while :; do sleep 1; done",
    ));
    let (sub, mut rx) = attach_sub(&mut task, false);
    wait_for_focus_reporting(&task).await;

    assert!(
        task.handle_input(sub, InputMsg::FocusChange { focused: true })
            .is_none()
    );
    assert!(
        task.pty_steps.contains(&PtyStep::FocusReport),
        "the report is written on the input, ahead of the fence: {:?}",
        task.pty_steps,
    );
    assert!(fence(&mut task, sub).is_none());

    assert_eq!(fence_replies(&drain_events(&mut rx)).len(), 1);
    wait_for_drained_writer(&task).await;
}

/// The same change on a saturated writer: the report is dropped,
/// and the fence completes anyway. A barrier that waited on a
/// best-effort report would hang on a child that stopped reading.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_focus_report_does_not_hold_back_the_fence() {
    let mut task = SessionTask::for_tests(session_with(
        "printf '\\033[?1004h'; while :; do sleep 1; done",
    ));
    let (sub, mut rx) = attach_sub(&mut task, false);
    wait_for_focus_reporting(&task).await;
    saturate_writer(&task);

    assert!(
        task.handle_input(sub, InputMsg::FocusChange { focused: true })
            .is_none()
    );
    assert!(fence(&mut task, sub).is_none());

    assert_eq!(
        fence_replies(&drain_events(&mut rx)).len(),
        1,
        "the fence answers on admission, not on the report landing",
    );
}

async fn wait_for_focus_reporting(task: &SessionTask) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !task.session.lock_core().grid.focus_reporting() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the child never enabled DECSET 1004",
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_drained_writer(task: &SessionTask) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while task.session.writer.pending_bytes() != 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the focus report never reached the PTY",
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Fills the unreserved gauge, which is what makes the next
/// generated report a dropped one. Written in chunks because the
/// OS writer drains into the PTY buffer while this runs.
fn saturate_writer(task: &SessionTask) {
    for _ in 0..64 {
        match task
            .session
            .writer
            .write_owned(vec![0u8; felis_pty::PTY_WRITE_PENDING_CAP / 4], None)
            .expect("the writer is live")
        {
            felis_pty::WriteOutcome::Queued => {}
            felis_pty::WriteOutcome::Dropped { .. } => return,
        }
    }
    panic!("the unreserved gauge never reached its cap");
}

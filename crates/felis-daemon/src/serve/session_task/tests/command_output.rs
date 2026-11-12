use super::*;

fn announce(task: &mut SessionTask, sub: SubscriberId, enabled: bool) {
    assert!(
        task.handle_input(sub, InputMsg::AnnounceOutput { enabled })
            .is_none()
    );
}

fn run_command(task: &SessionTask, output: &str) {
    write_to_grid(
        task,
        format!("\x1b]133;C\x07{output}\r\n\x1b]133;D;0\x07").as_bytes(),
    );
}

fn announced(events: &[OutEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|ev| match ev {
            OutEvent::Grid(GridMsg::CommandOutput { text, .. }) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn captures(task: &SessionTask) -> bool {
    task.session.lock_core().grid.capture_command_output()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_output_is_charged_its_text_against_the_backlog_cap() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (on, _rx) = attach_sub(&mut task, false);
    announce(&mut task, on, true);
    let text = "x".repeat(felis_protocol::messages::MAX_ANNOUNCE_BYTES);
    let ev = OutEvent::Grid(GridMsg::CommandOutput {
        text: text.clone(),
        omitted: felis_protocol::messages::OmittedOutput::None,
    });
    assert_eq!(ev.approx_wire_len(), text.len() + WIRE_LEN_OVERHEAD);

    let idx = task.sub_index(on).expect("subscriber present");
    task.subs[idx]
        .buffered
        .store(SUBSCRIBER_BUFFER_CAP - text.len(), Ordering::Relaxed);
    assert!(
        !task.subs[idx].push(ev),
        "an announcement past the backlog cap evicts like any bulk frame"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grid_capture_follows_whether_any_subscriber_announces() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (a, _rx_a) = attach_sub(&mut task, false);
    let (b, _rx_b) = attach_sub(&mut task, false);
    assert!(!captures(&task), "a fresh attachment does not announce");

    announce(&mut task, a, true);
    assert!(captures(&task));
    announce(&mut task, b, true);
    announce(&mut task, a, false);
    assert!(captures(&task), "b still announces");
    task.remove_sub(b);
    assert!(!captures(&task), "the last announcing subscriber left");

    announce(&mut task, a, true);
    assert!(captures(&task));
    task.remove_sub(a);
    assert!(!captures(&task));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_output_reaches_only_announcing_subscribers_at_their_minor() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (on, mut rx_on) = attach_sub(&mut task, false);
    let (_off, mut rx_off) = attach_sub(&mut task, false);
    let (old, mut rx_old) = attach_sub(&mut task, false);
    announce(&mut task, on, true);
    announce(&mut task, old, true);
    let old_idx = task.sub_index(old).expect("old subscriber present");
    task.subs[old_idx].minor = GridMsg::CommandOutput {
        text: String::new(),
        omitted: felis_protocol::messages::OmittedOutput::None,
    }
    .requires()
    .minor
        - 1;
    for rx in [&mut rx_on, &mut rx_off, &mut rx_old] {
        drain_events(rx);
    }

    run_command(&task, "hello");
    task.drain_and_fan().expect("effects drain");

    assert_eq!(announced(&drain_events(&mut rx_on)), ["hello"]);
    assert!(
        announced(&drain_events(&mut rx_off)).is_empty(),
        "a subscriber that did not opt in is never sent it"
    );
    assert!(
        announced(&drain_events(&mut rx_old)).is_empty(),
        "a connection below the arm's minor is never sent it"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_attach_replays_no_past_command_output() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (on, _rx_on) = attach_sub(&mut task, true);
    announce(&mut task, on, true);
    run_command(&task, "hello");
    task.drain_and_fan().expect("effects drain");

    let burst = attach_burst(&mut task, ConnectionMode::Window);
    assert!(
        announced(&burst).is_empty(),
        "an announcement is an event, not rehydrated state"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unpulled_subscriber_keeps_only_the_youngest_command_outputs() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (on, mut rx) = attach_sub(&mut task, true);
    announce(&mut task, on, true);
    drain_events(&mut rx);

    for n in 0..ANNOUNCE_QUEUE_CAP + 2 {
        run_command(&task, &n.to_string());
        task.drain_and_fan().expect("effects drain");
    }
    assert!(
        announced(&drain_events(&mut rx)).is_empty(),
        "nothing ships before the pull"
    );
    assert!(task.handle_input(on, InputMsg::NextGridFrame).is_none());

    let expected: Vec<String> = (2..ANNOUNCE_QUEUE_CAP + 2).map(|n| n.to_string()).collect();
    assert_eq!(announced(&drain_events(&mut rx)), expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opting_in_excludes_output_finished_before_the_request() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (early, mut rx_early) = attach_sub(&mut task, false);
    let (late, mut rx_late) = attach_sub(&mut task, false);
    announce(&mut task, early, true);
    drain_events(&mut rx_early);
    drain_events(&mut rx_late);

    run_command(&task, "before");
    announce(&mut task, late, true);
    run_command(&task, "after");
    task.drain_and_fan().expect("effects drain");

    assert_eq!(announced(&drain_events(&mut rx_early)), ["before", "after"]);
    assert_eq!(announced(&drain_events(&mut rx_late)), ["after"]);
}

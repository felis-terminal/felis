use super::*;

/// Per-client clipboard scope (security-model.md): an OSC 52 write
/// reaches only the active subscriber, never a mirror.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn osc52_write_reaches_only_the_active_subscriber() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (sub_a, mut rx_a) = attach_sub(&mut task, false);
    let (_sub_b, mut rx_b) = attach_sub(&mut task, false);
    task.active_sub = Some(sub_a);

    {
        let mut core = task.session.lock_core();
        let core = &mut *core;
        core.parser.advance(&mut core.grid, b"\x1b]52;c;Zm9v\x1b\\");
    }
    task.broadcast_facets();

    let writes_a = drain_events(&mut rx_a)
        .iter()
        .filter(|ev| matches!(ev, OutEvent::Grid(GridMsg::ClipboardSet { .. })))
        .count();
    assert_eq!(writes_a, 1, "the active subscriber hears the write");
    assert!(
        drain_events(&mut rx_b)
            .iter()
            .all(|ev| !matches!(ev, OutEvent::Grid(GridMsg::ClipboardSet { .. }))),
        "a mirror must not hear another client's clipboard write",
    );
}

/// With no active subscriber and several mirrors an OSC 52 write is
/// dropped; a sole window subscriber still receives it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unattributable_osc52_write_is_dropped_not_broadcast() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (_sub_a, mut rx_a) = attach_sub(&mut task, false);
    let (_sub_b, mut rx_b) = attach_sub(&mut task, false);

    {
        let mut core = task.session.lock_core();
        let core = &mut *core;
        core.parser.advance(&mut core.grid, b"\x1b]52;c;Zm9v\x1b\\");
    }
    task.broadcast_facets();

    for rx in [&mut rx_a, &mut rx_b] {
        assert!(
            drain_events(rx)
                .iter()
                .all(|ev| !matches!(ev, OutEvent::Grid(GridMsg::ClipboardSet { .. }))),
            "an unattributable write must reach no mirror",
        );
    }

    let mut solo = SessionTask::for_tests(session_with("read _x"));
    let (_only, mut rx_only) = attach_sub(&mut solo, false);
    {
        let mut core = solo.session.lock_core();
        let core = &mut *core;
        core.parser.advance(&mut core.grid, b"\x1b]52;c;Zm9v\x1b\\");
    }
    solo.broadcast_facets();
    let writes = drain_events(&mut rx_only)
        .iter()
        .filter(|ev| matches!(ev, OutEvent::Grid(GridMsg::ClipboardSet { .. })))
        .count();
    assert_eq!(writes, 1, "a sole window subscriber is unambiguous");
}

/// A burst drains as one write per selection; losing the active
/// window on the first must not hand the second to a bystander.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn osc52_writes_stop_when_the_active_subscriber_dies_mid_burst() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (sub_a, rx_a) = attach_sub(&mut task, false);
    let (_sub_b, mut rx_b) = attach_sub(&mut task, false);
    task.active_sub = Some(sub_a);
    drop(rx_a);

    write_to_grid(&task, b"\x1b]52;c;Zm9v\x1b\\\x1b]52;p;YmFy\x1b\\");
    task.broadcast_facets();

    assert!(
        drain_events(&mut rx_b)
            .iter()
            .all(|ev| !matches!(ev, OutEvent::Grid(GridMsg::ClipboardSet { .. }))),
        "a bystander must not inherit the dead window's clipboard write",
    );
}

/// The notification's attention push runs before the clipboard
/// push in a cycle; evicting the active window there must not hand
/// its write to the window left behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn osc52_write_is_not_inherited_after_an_earlier_push_evicts_the_active_window() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (sub_a, rx_a) = attach_sub(&mut task, false);
    let (_sub_b, mut rx_b) = attach_sub(&mut task, false);
    task.active_sub = Some(sub_a);
    drop(rx_a);

    write_to_grid(&task, b"\x1b]9;done\x07\x1b]52;c;Zm9v\x1b\\");
    task.after_drive();

    let events = drain_events(&mut rx_b);
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, OutEvent::Grid(GridMsg::Attention { .. }))),
        "the cycle reached the bystander, so the ordering was exercised",
    );
    assert!(
        events
            .iter()
            .all(|ev| !matches!(ev, OutEvent::Grid(GridMsg::ClipboardSet { .. }))),
        "a bystander must not inherit the evicted window's clipboard write",
    );
}

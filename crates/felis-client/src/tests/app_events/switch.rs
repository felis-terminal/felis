use super::*;

/// Pins the parked-push contract on `App::pending_reattach`: the
/// daemon already told the CLI the switch succeeded, so a push that
/// arrives mid-switch must run once that switch fails, not vanish.
#[test]
fn a_reattach_parked_behind_a_failing_switch_runs_when_it_fails() {
    let runtime = runtime();
    let mut harness = app_on(&runtime, absent_daemon("home"), 0xa);

    harness.app.on_app_event(AppEvent::ReattachRequested {
        reconnector: absent_daemon("first"),
        id: 0xb,
    });
    assert!(harness.app.switch_state.in_flight());

    harness.app.on_app_event(AppEvent::ReattachRequested {
        reconnector: absent_daemon("second"),
        id: 0xc,
    });
    assert_eq!(
        harness
            .app
            .pending_reattach
            .as_ref()
            .map(|pending| pending.id),
        Some(0xc),
        "a push during a switch is parked"
    );

    let failed = harness.next_posted();
    assert!(matches!(failed, AppEvent::SessionSwitchFailed { .. }));
    harness.app.on_app_event(failed);
    assert!(
        harness.app.pending_reattach.is_none(),
        "the parked push left the slot"
    );
    assert!(
        harness.app.switch_state.in_flight(),
        "the parked push is dialing"
    );

    let failed = harness.next_posted();
    assert!(matches!(failed, AppEvent::SessionSwitchFailed { .. }));
    harness.app.on_app_event(failed);
    assert_eq!(harness.app.switch_state, SwitchState::Idle);
    assert_eq!(harness.app.current_place().session_id, 0xa);
    assert!(!harness.app.exit_requested);
}

/// Pins the criterion "retry bounds are visible": a drop with
/// nothing in flight keeps the window and says so in its title.
#[test]
fn a_dropped_connection_marks_the_window_disconnected() {
    let runtime = runtime();
    let mut harness = app_on(&runtime, absent_daemon("home"), 0xa);

    let live = harness.app.conn_gen;
    harness
        .app
        .on_app_event(AppEvent::DaemonClosed { conn_gen: live });

    assert!(harness.app.reconnecting);
    assert_eq!(
        harness.window.last_title().as_deref(),
        Some("felis \u{2014} disconnected")
    );
    assert!(!harness.app.exit_requested);
}

/// Pins the run/pipe return path: the transient's close, absorbed
/// while the return switch was in flight, must not tear down the
/// connection that switch lands on.
#[cfg(unix)]
#[test]
fn a_switch_landing_off_a_lost_connection_stays_on_the_new_one() {
    let runtime = runtime();
    let mut harness = app_on(&runtime, absent_daemon("home"), 0xa);
    harness
        .app
        .outgoing
        .as_ref()
        .expect("the harness connection")
        .mark_lost();

    let _far = land_switch(&runtime, &mut harness, 0xb);

    let live = harness.app.conn_gen;
    while let Ok(event) = harness.posted.try_recv() {
        assert!(
            !matches!(event, AppEvent::DaemonClosed { conn_gen } if conn_gen == live),
            "the departing connection's refused Detach reported the new connection closed"
        );
    }
    assert!(!harness.app.reconnecting);
    assert_eq!(harness.app.current_place().session_id, 0xb);
}

#[cfg(unix)]
#[test]
fn a_switch_landing_detaches_the_departing_connection() {
    let runtime = runtime();
    let mut harness = app_on(&runtime, absent_daemon("home"), 0xa);
    harness.sent();

    let _far = land_switch(&runtime, &mut harness, 0xb);

    let detach = OutgoingFrame::ordered(&SessionToDaemonMsg::Detach)
        .expect("encodes")
        .frame()
        .clone();
    assert_eq!(harness.sent(), vec![detach]);
}

/// Pins the `conn_gen` stamp: a BEL stamped by any connection but
/// the live one must not flash the window.
#[test]
fn only_the_live_connection_rings_the_bell() {
    let runtime = runtime();
    let mut harness = app_on(&runtime, absent_daemon("home"), 0xa);
    let bell = || GridMsg::Attention {
        source: felis_protocol::messages::AttentionSource::Bell,
    };

    let live = harness.app.conn_gen;
    harness.app.on_app_event(AppEvent::GridFrame {
        msg: bell(),
        conn_gen: live + 1,
    });
    assert_eq!(harness.window.attention_requests(), 0);

    harness.app.on_app_event(AppEvent::GridFrame {
        msg: bell(),
        conn_gen: live,
    });
    assert_eq!(harness.window.attention_requests(), 1);
}

fn retarget_target() -> RetargetTarget {
    RetargetTarget {
        carrier: felis_protocol::messages::RetargetCarrier::LocalEndpoint(
            "/nonexistent/felis-test-retarget.sock".into(),
        ),
        landing: felis_protocol::messages::RetargetLanding::Attach("b".into()),
    }
}

/// Pins the one-slot rule on `App::pending_reattach` and
/// `App::pending_retarget`: whichever push arrived last is the one a
/// landing runs.
#[test]
fn the_last_push_parked_during_a_switch_is_the_one_kept() {
    let runtime = runtime();
    let mut harness = app_on(&runtime, absent_daemon("home"), 0xa);
    harness.app.on_app_event(AppEvent::ReattachRequested {
        reconnector: absent_daemon("first"),
        id: 0xb,
    });
    assert!(harness.app.is_busy());

    harness.app.on_app_event(AppEvent::ReattachRequested {
        reconnector: absent_daemon("second"),
        id: 0xc,
    });
    harness.app.on_app_event(AppEvent::RetargetRequested {
        target: retarget_target(),
    });
    assert!(harness.app.pending_reattach.is_none());
    assert_eq!(harness.app.pending_retarget, Some(retarget_target()));

    harness.app.on_app_event(AppEvent::ReattachRequested {
        reconnector: absent_daemon("third"),
        id: 0xd,
    });
    assert!(harness.app.pending_retarget.is_none());
    assert_eq!(
        harness
            .app
            .pending_reattach
            .as_ref()
            .map(|pending| pending.id),
        Some(0xd)
    );
}

/// The daemon declines a same-session switch, so a push naming the
/// window's own place is one the window reached while it was in
/// flight; re-attaching would rehydrate onto the same grid.
#[test]
fn a_reattach_push_to_the_current_place_dials_nothing() {
    let runtime = runtime();
    let home = absent_daemon("home");
    let mut harness = app_on(&runtime, home.clone(), 0xa);

    harness.app.on_app_event(AppEvent::ReattachRequested {
        reconnector: home,
        id: 0xa,
    });

    assert_eq!(harness.app.switch_state, SwitchState::Idle);
    assert!(harness.app.pending_reattach.is_none());
}

/// Pins `SwitchState::carry_exit` at the window: a shell exit during a
/// landing starts no exit ladder of its own (it would dial beside the
/// landing), and the ladder starts when that landing fails, ending in
/// a close once every rung is spent.
#[test]
fn a_shell_exit_during_a_switch_waits_for_the_landing_then_unwinds_to_a_close() {
    let runtime = runtime();
    let mut harness = app_on(&runtime, absent_daemon("home"), 0xa);
    harness.app.on_app_event(AppEvent::ReattachRequested {
        reconnector: absent_daemon("elsewhere"),
        id: 0xb,
    });

    let live = harness.app.conn_gen;
    let place = harness.app.current_place();
    harness.app.on_app_event(AppEvent::SessionExited {
        conn_gen: live,
        place,
    });
    assert!(harness.app.switch_state.session_gone());
    assert!(
        harness.app.exit_ladder.is_none(),
        "the landing carries the exit"
    );

    // Every rung dials a daemon nobody runs; each failure is the
    // next rung's cue, and the ladder's rungs are finite.
    for _ in 0..8 {
        if harness.app.exit_requested {
            break;
        }
        let failed = harness.next_posted();
        assert!(matches!(failed, AppEvent::SessionSwitchFailed { .. }));
        harness.app.on_app_event(failed);
    }
    assert!(harness.app.exit_requested);
}

/// A refusal nobody cleared would leave the pending-switch slot armed,
/// and every later chord queued behind it for the rest of the
/// connection; one stamped by another connection is not this fetch's.
#[test]
fn a_refused_roster_fetch_frees_the_next_switch_chord() {
    let runtime = runtime();
    let mut harness = app_on(&runtime, absent_daemon("home"), 0xa);
    harness.app.switch_session(SwitchDirection::Next);
    let request = felis_protocol::messages::RequestId::new(1).expect("nonzero");
    assert!(harness.app.pending_switch.awaiting(request));

    let live = harness.app.conn_gen;
    harness.app.on_app_event(AppEvent::RequestRefused {
        request,
        detail: "stale".into(),
        conn_gen: live + 1,
    });
    assert!(
        harness.app.is_busy(),
        "another connection's refusal is ignored"
    );

    harness.app.on_app_event(AppEvent::RequestRefused {
        request,
        detail: "refused".into(),
        conn_gen: live,
    });
    assert!(!harness.app.is_busy());

    harness.sent();
    harness.app.switch_session(SwitchDirection::Next);
    assert!(harness.app.is_busy(), "the next chord fetches again");
    assert_eq!(harness.sent().len(), 1, "with a roster request of its own");
}

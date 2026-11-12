use super::*;

/// A-5: one shared sequence would make an inactive low id ambiguous
/// between a terminated stream and a request.
#[test]
fn the_two_id_sequences_are_independent_and_sequential() {
    let mut driver = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Ops);
    let mut requests = Vec::new();
    let mut streams = Vec::new();
    for _ in 0..5 {
        requests.push(driver.issue_request().expect("the sequence is fresh").get());
        streams.push(driver.open_stream().expect("the sequence is fresh").get());
        requests.push(driver.issue_request().expect("the sequence is fresh").get());
    }
    assert_eq!(requests, (1..=10).collect::<Vec<_>>());
    assert_eq!(streams, (1..=5).collect::<Vec<_>>());
}

/// A stale cancel must never reach a later stream that reused the id.
#[test]
fn a_terminated_stream_id_is_never_reissued() {
    let mut driver = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let first = driver.open_stream().expect("the sequence is fresh");
    let terminal = frame(&ConnToClientMsg::End {
        stream_id: first,
        count: 0,
    });
    drop(driver.classify(&terminal).unwrap());
    assert_eq!(driver.classify_stream(first), StreamClass::Terminated);

    let second = driver.open_stream().expect("the sequence is fresh");
    assert_ne!(second, first);
    assert_eq!(second.get(), first.get() + 1);
}

// ── stream classification ───────────────────────────────────────────

#[test]
fn stream_ids_classify_as_active_canceled_terminated_or_never_opened() {
    let mut driver = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let live = driver.open_stream().expect("the sequence is fresh");
    let canceled = driver.open_stream().expect("the sequence is fresh");
    let finished = driver.open_stream().expect("the sequence is fresh");

    driver.cancel_stream(canceled);
    drop(
        driver
            .classify(&frame(&ConnToClientMsg::End {
                stream_id: finished,
                count: 3,
            }))
            .unwrap(),
    );

    assert_eq!(driver.classify_stream(live), StreamClass::Active);
    assert_eq!(driver.classify_stream(canceled), StreamClass::Canceled);
    assert_eq!(driver.classify_stream(finished), StreamClass::Terminated);
    assert_eq!(
        driver.classify_stream(stream(finished.get() + 1)),
        StreamClass::NeverOpened
    );
}

#[test]
fn an_open_with_the_wrong_next_stream_id_closes_the_connection() {
    let mut driver = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
    let f = frame_correlated(&a_query(), Correlation::stream(stream(4)));
    let err = decode_frame::<SearchToDaemonMsg>(&mut driver, &f).unwrap_err();
    assert!(
        matches!(
            &err,
            DriverError::Correlation { kind: MessageKind::Search, expected, found }
                if expected.contains("next unopened") && found.contains("stream 4")
        ),
        "got {err}"
    );
}

#[test]
fn reopening_a_terminated_stream_id_closes_the_connection() {
    let mut driver = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
    open_on_daemon(&mut driver, stream(1));
    drop(driver.finish_stream(stream(1), 0).expect("live stream"));

    let f = frame_correlated(&a_query(), Correlation::stream(stream(1)));
    assert!(matches!(
        decode_frame::<SearchToDaemonMsg>(&mut driver, &f).unwrap_err(),
        DriverError::Correlation { .. }
    ));
}

// ── cancel / terminal races ─────────────────────────────────────────

/// Treating post-cancel items as corruption would kill a connection for
/// obeying the protocol.
#[test]
fn items_arriving_after_a_local_cancel_are_dropped_not_fatal() {
    let mut driver = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let id = driver.open_stream().expect("the sequence is fresh");
    driver.cancel_stream(id);

    let f = frame_correlated(&a_match(), Correlation::stream(id));
    assert!(matches!(
        decode_frame::<SearchToClientMsg>(&mut driver, &f).unwrap(),
        Delivery::DroppedAfterCancel
    ));

    assert_eq!(driver.classify_stream(id), StreamClass::Canceled);
    drop(
        driver
            .classify(&frame(&ConnToClientMsg::End {
                stream_id: id,
                count: 1,
            }))
            .unwrap(),
    );
    assert_eq!(driver.classify_stream(id), StreamClass::Terminated);
}

#[test]
fn a_cancel_for_a_terminated_stream_is_an_idempotent_no_op() {
    let mut driver = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
    open_on_daemon(&mut driver, stream(1));
    drop(driver.finish_stream(stream(1), 2).expect("live stream"));

    let cancel = frame(&ConnToDaemonMsg::Cancel {
        stream_id: stream(1),
    });
    assert!(matches!(
        driver.classify(&cancel).unwrap(),
        Incoming::CancelIgnored { stream_id } if stream_id == stream(1)
    ));
    assert!(matches!(
        driver.classify(&cancel).unwrap(),
        Incoming::CancelIgnored { .. }
    ));
}

#[test]
fn a_cancel_for_a_never_opened_stream_closes_the_connection() {
    let mut driver = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
    let err = driver
        .classify(&frame(&ConnToDaemonMsg::Cancel {
            stream_id: stream(9),
        }))
        .unwrap_err();
    assert!(
        matches!(&err, DriverError::Correlation { found, .. } if found.contains("never opened")),
        "got {err}"
    );
}

#[test]
fn cancelling_a_live_stream_twice_is_accepted() {
    let mut driver = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
    open_on_daemon(&mut driver, stream(1));
    let cancel = frame(&ConnToDaemonMsg::Cancel {
        stream_id: stream(1),
    });
    for _ in 0..2 {
        assert!(matches!(
            driver.classify(&cancel).unwrap(),
            Incoming::Control(ConnToDaemonMsg::Cancel { .. })
        ));
    }
    assert_eq!(driver.classify_stream(stream(1)), StreamClass::Canceled);
}

// ── exactly one terminal ────────────────────────────────────────────

/// A collecting loop reads its completion from exactly one terminal.
/// The client's correlated wait re-feeds frames it read past, and a frame
/// counted twice would settle a stream transition twice, so a second feed
/// of one terminal is a correlation violation, never a second delivery.
#[test]
fn a_second_terminal_closes_the_connection() {
    let mut driver = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let id = driver.open_stream().expect("the sequence is fresh");
    let end = frame(&ConnToClientMsg::End {
        stream_id: id,
        count: 0,
    });
    drop(driver.classify(&end).unwrap());
    let err = driver.classify(&end).unwrap_err();
    assert!(
        matches!(&err, DriverError::Correlation { expected, .. } if expected.contains("one terminal")),
        "got {err}"
    );
}

#[test]
fn an_error_terminal_also_closes_the_stream_for_good() {
    let mut driver = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let id = driver.open_stream().expect("the sequence is fresh");
    drop(
        driver
            .classify(&frame(&ConnToClientMsg::Error {
                subject: Subject::Stream(id),
                reason: StreamErrorReason::InvalidRequest,
                detail: "empty needle".into(),
            }))
            .unwrap(),
    );
    assert_eq!(driver.classify_stream(id), StreamClass::Terminated);
    assert!(
        driver
            .classify(&frame(&ConnToClientMsg::End {
                stream_id: id,
                count: 0
            }))
            .is_err()
    );
}

#[test]
fn an_item_after_the_terminal_closes_the_connection() {
    let mut driver = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let id = driver.open_stream().expect("the sequence is fresh");
    drop(
        driver
            .classify(&frame(&ConnToClientMsg::End {
                stream_id: id,
                count: 0,
            }))
            .unwrap(),
    );
    let f = frame_correlated(&a_match(), Correlation::stream(id));
    let err = decode_frame::<SearchToClientMsg>(&mut driver, &f).unwrap_err();
    assert!(
        matches!(&err, DriverError::Correlation { found, .. } if found.contains("Terminated")),
        "got {err}"
    );
}

#[test]
fn a_terminal_for_a_never_opened_stream_closes_the_connection() {
    let mut driver = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    assert!(
        driver
            .classify(&frame(&ConnToClientMsg::End {
                stream_id: stream(3),
                count: 0
            }))
            .is_err()
    );
}

// ── region and notify streams ───────────────────────────────────────

#[test]
fn every_streaming_family_shares_one_table() {
    let mut driver = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
    let rows = frame_correlated(
        &RegionToDaemonMsg::Rows {
            source: RegionSource::Scrollback,
            ansi: false,
            max_rows: None,
            session: None,
        },
        Correlation::stream(stream(1)),
    );
    assert!(matches!(
        decode_frame::<RegionToDaemonMsg>(&mut driver, &rows).unwrap(),
        Delivery::Deliver(_)
    ));

    let mut observer = in_phase::<DaemonSide>(Phase::Setup, ConnectionMode::Observer);
    let subscribe = frame_correlated(
        &NotifyToDaemonMsg::Subscribe {
            session_prefix: None,
            kinds: Vec::new(),
        },
        Correlation::stream(stream(1)),
    );
    assert!(matches!(
        decode_frame::<NotifyToDaemonMsg>(&mut observer, &subscribe).unwrap(),
        Delivery::Deliver(_)
    ));
    assert_eq!(observer.outstanding_streams(), 1);
}

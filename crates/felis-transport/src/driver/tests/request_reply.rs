use super::*;

/// The requests were issued in order but the replies may not be: a
/// slow verb must not hold up the answer to a later one.
#[test]
fn replies_match_their_requests_in_any_order() {
    let mut driver = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Ops);
    let first = driver.issue_request().expect("the sequence is fresh");
    let second = driver.issue_request().expect("the sequence is fresh");

    for id in [second, first] {
        let f = frame_correlated(
            &OpsToClientMsg::Listed { sessions: vec![] },
            Correlation::request(id),
        );
        let Delivery::Deliver(delivered) = decode_frame::<OpsToClientMsg>(&mut driver, &f).unwrap()
        else {
            panic!("a reply is delivered");
        };
        assert_eq!(delivered.request(), id);
    }
}

/// A second reply for a retired id, or one for an id never issued, is
/// corruption rather than a frame to drop: taken as an answer it would
/// hand the caller another request's result.
#[test]
fn a_duplicate_or_late_reply_closes_the_connection() {
    let mut driver = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Ops);
    let id = driver.issue_request().expect("the sequence is fresh");
    let listed = frame_correlated(
        &OpsToClientMsg::Listed { sessions: vec![] },
        Correlation::request(id),
    );

    assert!(decode_frame::<OpsToClientMsg>(&mut driver, &listed).is_ok());

    let err = decode_frame::<OpsToClientMsg>(&mut driver, &listed).unwrap_err();
    assert!(
        matches!(&err, DriverError::Correlation { found, .. } if found.contains("not outstanding")),
        "got {err}"
    );

    let never = frame_correlated(
        &OpsToClientMsg::Listed { sessions: vec![] },
        Correlation::request(RequestId::new(9).expect("nonzero")),
    );
    assert!(matches!(
        decode_frame::<OpsToClientMsg>(&mut driver, &never).unwrap_err(),
        DriverError::Correlation { .. }
    ));
}

/// A reply arm with no envelope at all: the class says a request id is
/// mandatory, so the driver refuses it instead of a handler recovering
/// one after the fact.
#[test]
fn a_reply_without_an_envelope_closes_the_connection() {
    let mut driver = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Ops);
    let _issued = driver.issue_request().expect("the sequence is fresh");
    let f = frame(&OpsToClientMsg::Listed { sessions: vec![] });
    let err = decode_frame::<OpsToClientMsg>(&mut driver, &f).unwrap_err();
    assert!(
        matches!(
            &err,
            DriverError::Correlation { expected, found, .. }
                if expected.contains("echoing an outstanding request")
                    && found.contains("no correlation envelope")
        ),
        "got {err}"
    );
}

#[test]
fn a_typed_refusal_retires_the_request_it_names() {
    let mut driver = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let id = driver.issue_request().expect("the sequence is fresh");

    let refusal = frame(&ConnToClientMsg::Error {
        subject: Subject::Request(id),
        reason: StreamErrorReason::InvalidRequest,
        detail: "a Window connection may not operate on other sessions".into(),
    });
    assert!(matches!(
        driver.classify(&refusal).unwrap(),
        Incoming::Control(ConnToClientMsg::Error { .. })
    ));

    assert!(driver.match_reply(MessageKind::Ops, id).is_err());
    assert!(driver.classify(&refusal).is_err());
}

// ── bounded streams ─────────────────────────────────────────────────

#[test]
fn a_stream_id_on_an_arm_that_opens_none_closes_the_connection() {
    let mut driver = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
    let f = frame_correlated(&OpsToDaemonMsg::List, Correlation::stream(stream(1)));
    let err = decode_frame::<OpsToDaemonMsg>(&mut driver, &f).unwrap_err();
    assert!(
        matches!(&err, DriverError::Correlation { found, .. } if found.contains("Ops::List")),
        "got {err}"
    );
    assert_eq!(driver.outstanding_streams(), 0);
}

/// The bound counts streams open at once, not opened ever.
#[test]
fn retiring_a_stream_frees_its_slot_for_the_next_one() {
    let mut driver =
        in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops).with_stream_bound(1);
    open_on_daemon(&mut driver, stream(1));
    assert!(driver.retire_stream(stream(1)));
    assert_eq!(driver.outstanding_streams(), 0);
    assert_eq!(driver.classify_stream(stream(1)), StreamClass::Terminated);

    open_on_daemon(&mut driver, stream(2));
    assert_eq!(driver.classify_stream(stream(2)), StreamClass::Active);
    assert!(!driver.retire_stream(stream(1)));
}

/// The refused id is still consumed, so the two ends' counters stay in
/// step.
#[test]
fn exceeding_the_stream_bound_refuses_the_request_not_the_connection() {
    let mut driver =
        in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops).with_stream_bound(2);
    open_on_daemon(&mut driver, stream(1));
    open_on_daemon(&mut driver, stream(2));

    let f = frame_correlated(&a_query(), Correlation::stream(stream(3)));
    let Delivery::RefuseStream { reply } =
        decode_frame::<SearchToDaemonMsg>(&mut driver, &f).unwrap()
    else {
        panic!("expected a refusal");
    };
    assert!(matches!(
        reply,
        ConnToClientMsg::Error {
            subject: Subject::Stream(id),
            reason: StreamErrorReason::TooManyStreams,
            ..
        } if id == stream(3)
    ));
    assert_eq!(driver.outstanding_streams(), 2);
    assert_eq!(driver.classify_stream(stream(3)), StreamClass::Terminated);

    drop(driver.finish_stream(stream(1), 0).expect("live"));
    open_on_daemon(&mut driver, stream(4));
    assert_eq!(driver.classify_stream(stream(4)), StreamClass::Active);
}

#[test]
fn the_stream_bound_is_per_connection() {
    let mut greedy =
        in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops).with_stream_bound(1);
    let mut neighbor =
        in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops).with_stream_bound(1);
    open_on_daemon(&mut greedy, stream(1));

    open_on_daemon(&mut neighbor, stream(1));
    assert_eq!(neighbor.outstanding_streams(), 1);
}

// ── isolation ───────────────────────────────────────────────────────

/// The structural half of A-7's scope rule; the live half is
/// `felis-daemon`'s `a_corrupt_frame_kills_its_own_connection_and_nothing_else`.
#[test]
fn one_connections_death_leaves_the_others_untouched() {
    let mut doomed = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
    let mut healthy = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
    open_on_daemon(&mut healthy, stream(1));

    assert!(
        doomed
            .classify(&OwnedFrame {
                kind: 4242,
                body: Bytes::new()
            })
            .is_err()
    );

    assert_eq!(healthy.classify_stream(stream(1)), StreamClass::Active);
    open_on_daemon(&mut healthy, stream(2));
    assert_eq!(healthy.outstanding_streams(), 2);
}

/// The daemon holds the same request counter the client allocates
/// from, so a gap is caught at the request rather than at whatever the
/// reply happens to answer.
#[test]
fn a_skipped_request_id_closes_the_connection() {
    let mut daemon = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
    for id in [1, 3] {
        let f = frame_correlated(&OpsToDaemonMsg::List, Correlation::request(request(id)));
        let outcome = decode_frame::<OpsToDaemonMsg>(&mut daemon, &f);
        if id == 1 {
            assert!(matches!(outcome, Ok(Delivery::Deliver(_))));
        } else {
            let err = outcome.unwrap_err();
            assert!(
                matches!(&err, DriverError::Correlation { found, .. }
                    if found.contains("request 3") && found.contains("2 was next")),
                "got {err}"
            );
        }
    }
}

/// Reusing a retired id would make one reply attributable to two
/// requests.
#[test]
fn a_reused_request_id_closes_the_connection() {
    let mut daemon = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
    let f = frame_correlated(&OpsToDaemonMsg::List, Correlation::request(request(1)));
    for expect_ok in [true, false] {
        assert_eq!(
            decode_frame::<OpsToDaemonMsg>(&mut daemon, &f).is_ok(),
            expect_ok
        );
    }
}

/// The two sequences advance independently: opening streams must not
/// consume request ids, and vice versa.
#[test]
fn the_request_and_stream_sequences_stay_independent_at_the_daemon() {
    let mut daemon = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
    open_on_daemon(&mut daemon, stream(1));
    let f = frame_correlated(&OpsToDaemonMsg::List, Correlation::request(request(1)));
    assert!(matches!(
        decode_frame::<OpsToDaemonMsg>(&mut daemon, &f).unwrap(),
        Delivery::Deliver(_)
    ));
    open_on_daemon(&mut daemon, stream(2));
}

/// The identity the driver validated travels with the message, so a
/// handler reads it back instead of re-deriving it from the envelope.
#[test]
fn a_delivery_carries_the_identity_its_class_declares() {
    let mut daemon = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
    let f = frame_correlated(&OpsToDaemonMsg::List, Correlation::request(request(1)));
    let Delivery::Deliver(delivered) = decode_frame::<OpsToDaemonMsg>(&mut daemon, &f).unwrap()
    else {
        panic!("delivered");
    };
    assert_eq!(delivered.request(), request(1));

    let f = frame_correlated(&a_query(), Correlation::stream(stream(1)));
    let Delivery::Deliver(delivered) = decode_frame::<SearchToDaemonMsg>(&mut daemon, &f).unwrap()
    else {
        panic!("delivered");
    };
    assert_eq!(delivered.stream(), stream(1));
}

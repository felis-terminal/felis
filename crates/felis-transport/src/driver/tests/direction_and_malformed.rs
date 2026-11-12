use super::*;

#[test]
fn a_reply_half_frame_from_a_peer_closes_the_connection() {
    let mut driver = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
    let f = frame(&OpsToClientMsg::Listed { sessions: vec![] });
    let Incoming::Payload(p) = driver.classify(&f).unwrap() else {
        panic!("payload");
    };
    let err = driver.decode::<OpsToDaemonMsg>(&p).unwrap_err();
    assert!(
        matches!(
            &err,
            DriverError::WrongDirection { arm, expected, actual }
                if *arm == "Ops::Listed"
                    && *expected == "daemon→client"
                    && *actual == "client→daemon"
        ),
        "got {err}"
    );
}

#[test]
fn a_one_way_family_arriving_backwards_closes_the_connection() {
    let mut daemon = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Window);
    let err = daemon
        .classify(&OwnedFrame {
            kind: MessageKind::Grid.as_u16(),
            body: Bytes::new(),
        })
        .unwrap_err();
    assert!(
        matches!(&err, DriverError::WrongDirection { arm, .. } if *arm == "Grid"),
        "got {err}"
    );

    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    assert!(matches!(
        client
            .classify(&OwnedFrame {
                kind: MessageKind::Input.as_u16(),
                body: Bytes::new(),
            })
            .unwrap_err(),
        DriverError::WrongDirection { .. }
    ));
}

#[test]
fn a_terminal_arriving_at_the_daemon_closes_the_connection() {
    let mut driver = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
    let err = driver
        .classify(&frame(&ConnToClientMsg::End {
            stream_id: stream(1),
            count: 0,
        }))
        .unwrap_err();
    assert!(
        matches!(&err, DriverError::WrongDirection { arm, .. } if *arm == "Conn::End"),
        "got {err}"
    );
}

/// One body holding an arm of each direction, in the given order. prost
/// reads either wrapper past the other's arm as an unknown field.
fn mixed(kind: MessageKind, parts: &[&[u8]]) -> OwnedFrame {
    OwnedFrame {
        kind: kind.as_u16(),
        body: Bytes::from(parts.concat()),
    }
}

fn is_wrong_direction(err: &DriverError, named: &str) -> bool {
    matches!(err, DriverError::WrongDirection { arm, .. } if *arm == named)
}

#[test]
fn a_hello_sharing_its_body_with_a_welcome_closes_the_connection() {
    let hello = codec::encode(&ConnToDaemonMsg::Hello {
        mode: ConnectionMode::Ops,
        pull_paced: false,
    });
    let welcome = codec::encode(&ConnToClientMsg::Welcome { identity: None });
    for parts in [[&hello, &welcome], [&welcome, &hello]] {
        let mut daemon = ConnectionDriver::daemon();
        daemon.preface_done();
        let err = daemon
            .classify(&mixed(MessageKind::Conn, &[parts[0], parts[1]]))
            .unwrap_err();
        assert!(is_wrong_direction(&err, "Conn::Welcome"), "got {err}");
    }
}

#[test]
fn a_request_sharing_its_body_with_a_reply_closes_the_connection() {
    let list = correlated(&OpsToDaemonMsg::List, Correlation::request(request(1))).body;
    let listed = codec::encode(&OpsToClientMsg::Listed { sessions: vec![] });
    for parts in [[&list[..], &listed], [&listed, &list[..]]] {
        let mut daemon = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
        let Incoming::Payload(p) = daemon.classify(&mixed(MessageKind::Ops, &parts)).unwrap()
        else {
            panic!("payload");
        };
        let err = daemon.decode::<OpsToDaemonMsg>(&p).unwrap_err();
        assert!(is_wrong_direction(&err, "Ops::Listed"), "got {err}");
    }
}

/// The drain judges a frame it never delivers, and a request hidden
/// behind a reply it would have dropped is the same fault.
#[test]
fn a_drained_reply_sharing_its_body_with_a_request_closes_the_connection() {
    let found = correlated(&a_match(), Correlation::stream(stream(1))).body;
    let query = codec::encode(&a_query());
    for parts in [[&found[..], &query], [&query, &found[..]]] {
        let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
        let Incoming::Payload(p) = client
            .classify(&mixed(MessageKind::Search, &parts))
            .unwrap()
        else {
            panic!("payload");
        };
        let err = client.admit_drained(&p).unwrap_err();
        assert!(is_wrong_direction(&err, "Search::Query"), "got {err}");
    }
}

/// prost steps over a group in a field it does not know; the walk that
/// looks for the other direction's arms does not, and no felis message
/// declares one.
#[test]
fn a_body_the_arm_walk_cannot_read_closes_the_connection() {
    let hello = codec::encode(&ConnToDaemonMsg::Hello {
        mode: ConnectionMode::Ops,
        pull_paced: false,
    });
    let group: &[u8] = &[0x93, 0x03, 0x94, 0x03];
    let mut daemon = ConnectionDriver::daemon();
    daemon.preface_done();
    let err = daemon
        .classify(&mixed(MessageKind::Conn, &[&hello, group]))
        .unwrap_err();
    assert!(
        matches!(
            &err,
            DriverError::UndecodableBody {
                expected: MessageKind::Conn,
                ..
            }
        ),
        "got {err}"
    );
}

// ── malformed frames ────────────────────────────────────────────────

/// A log line saying only "decode failed" cannot tell version skew from
/// a corrupt socket.
#[test]
fn an_undecodable_body_closes_the_connection_naming_expected_and_actual() {
    // An un-correlated family, so the failure is the body decode itself.
    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let f = OwnedFrame {
        kind: MessageKind::Grid.as_u16(),
        body: Bytes::from_static(&[0xFF, 0xFF, 0xFF]),
    };
    let Incoming::Payload(p) = client.classify(&f).unwrap() else {
        panic!("payload");
    };
    let err = client.decode::<GridMsg>(&p).unwrap_err();
    let DriverError::UndecodableBody { expected, found } = &err else {
        panic!("got {err}");
    };
    assert_eq!(*expected, MessageKind::Grid);
    assert!(!found.is_empty(), "the decoder's account must survive");

    // A body that parses as protobuf but leaves the oneof unset.
    let mut daemon = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
    let f = OwnedFrame {
        kind: MessageKind::Ops.as_u16(),
        body: Bytes::new(),
    };
    let Incoming::Payload(p) = daemon.classify(&f).unwrap() else {
        panic!("payload");
    };
    assert!(matches!(
        daemon.decode::<OpsToDaemonMsg>(&p).unwrap_err(),
        DriverError::UndecodableBody {
            expected: MessageKind::Ops,
            ..
        }
    ));
}

/// The envelope is lifted off every body, so a body that is not
/// protobuf at all fails that peek first. It is still reported as the
/// family failing to parse: naming the envelope would send every
/// reader of the log looking for a correlation bug.
#[test]
fn an_unparseable_correlated_body_is_reported_as_its_family() {
    let mut driver = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
    let Incoming::Payload(p) = driver
        .classify(&OwnedFrame {
            kind: MessageKind::Ops.as_u16(),
            body: Bytes::from_static(&[0xFF, 0xFF, 0xFF]),
        })
        .unwrap()
    else {
        panic!("payload");
    };
    let err = driver.decode::<OpsToDaemonMsg>(&p).unwrap_err();
    assert!(
        matches!(
            &err,
            DriverError::UndecodableBody {
                expected: MessageKind::Ops,
                ..
            }
        ),
        "got {err}"
    );
}

/// The length prefix would let the reader skip the frame, but a peer
/// speaking an unparseable family cannot be followed.
#[test]
fn an_unknown_kind_closes_the_connection() {
    let mut driver = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
    let err = driver
        .classify(&OwnedFrame {
            kind: 999,
            body: Bytes::new(),
        })
        .unwrap_err();
    assert!(
        matches!(&err, DriverError::UnexpectedKind { found, .. } if found.contains("999")),
        "got {err}"
    );
}

/// Hand-assembled, since no encoder produces one: `OpsToDaemonMsg.list` (field
/// 1, empty) then `correlation` (field 100, empty).
#[test]
fn an_empty_correlation_envelope_closes_the_connection() {
    let mut driver = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
    let body: &[u8] = &[0x0A, 0x00, 0xA2, 0x06, 0x00];
    let err = driver
        .classify(&OwnedFrame {
            kind: MessageKind::Ops.as_u16(),
            body: Bytes::from_static(body),
        })
        .unwrap_err();
    assert!(matches!(err, DriverError::Correlation { .. }), "got {err}");
}

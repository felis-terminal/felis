//! The driver's contract (A-5/A-7), pinned case by case.

use felis_protocol::{
    ConnectionMode, ImageId, MessageKind, RowPayload,
    codec::{self, WireCodec},
    messages::{
        ConnToClientMsg, ConnToDaemonMsg, Correlation, GridMsg, ImageFormat, ImageMsg, ImageTarget,
        InputMsg, NotifyToClientMsg, NotifyToDaemonMsg, OpsToClientMsg, OpsToDaemonMsg, PushMsg,
        RegionSource, RegionToClientMsg, RegionToDaemonMsg, RequestId, SearchOptions,
        SearchToClientMsg, SearchToDaemonMsg, SessionToClientMsg, SessionToDaemonMsg,
        StreamErrorReason, StreamId, Subject,
    },
};

use super::*;

/// A driver welcomed and driven to `phase`. The transitions are the
/// driver's own, so a test cannot place it in a phase a connection
/// could not reach.
fn in_phase<R: Role>(phase: Phase, mode: ConnectionMode) -> ConnectionDriver<R> {
    let mut driver = ConnectionDriver::<R>::fresh(None);
    driver.preface_done();
    driver.handshake_done(mode);
    match phase {
        Phase::Setup => {}
        Phase::Attached => driver.attached(),
        Phase::Observing => driver.observing(),
        Phase::Preface | Phase::Handshake => {
            panic!("in_phase starts from a welcomed connection")
        }
    }
    driver
}

fn frame<M: WireCodec>(msg: &M) -> OwnedFrame {
    OwnedFrame {
        kind: M::KIND.as_u16(),
        body: Bytes::from(codec::encode(msg)),
    }
}

fn correlated<M: Correlated>(msg: &M, correlation: Correlation) -> OwnedFrame {
    frame_correlated(msg, correlation)
}

fn request(raw: u64) -> RequestId {
    RequestId::new(raw).expect("nonzero")
}

fn stream(raw: u64) -> StreamId {
    StreamId::new(raw).expect("nonzero")
}

fn a_query() -> SearchToDaemonMsg {
    SearchToDaemonMsg::Query {
        query: "needle".into(),
        options: SearchOptions::default(),
    }
}

fn a_match() -> SearchToClientMsg {
    SearchToClientMsg::Match {
        line_index: -1,
        text: "hit".into(),
        byte_spans: vec![],
        col_spans: vec![],
    }
}

fn open_on_daemon(driver: &mut DaemonDriver, id: StreamId) {
    let f = correlated(&a_query(), Correlation::stream(id));
    let Incoming::Payload(p) = driver.classify(&f).unwrap() else {
        panic!("a search query is a payload");
    };
    assert!(matches!(
        driver.decode::<SearchToDaemonMsg>(&p).unwrap(),
        Delivery::Deliver(_)
    ));
}

// ── id allocation ───────────────────────────────────────────────────

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
    let f = correlated(&a_query(), Correlation::stream(stream(4)));
    let Incoming::Payload(p) = driver.classify(&f).unwrap() else {
        panic!("payload");
    };
    let err = driver.decode::<SearchToDaemonMsg>(&p).unwrap_err();
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

    let f = correlated(&a_query(), Correlation::stream(stream(1)));
    let Incoming::Payload(p) = driver.classify(&f).unwrap() else {
        panic!("payload");
    };
    assert!(matches!(
        driver.decode::<SearchToDaemonMsg>(&p).unwrap_err(),
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

    let f = correlated(&a_match(), Correlation::stream(id));
    let Incoming::Payload(p) = driver.classify(&f).unwrap() else {
        panic!("payload");
    };
    assert!(matches!(
        driver.decode::<SearchToClientMsg>(&p).unwrap(),
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
    let f = correlated(&a_match(), Correlation::stream(id));
    let Incoming::Payload(p) = driver.classify(&f).unwrap() else {
        panic!("payload");
    };
    let err = driver.decode::<SearchToClientMsg>(&p).unwrap_err();
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

// ── direction ───────────────────────────────────────────────────────

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

// ── phase ───────────────────────────────────────────────────────────

/// The preface is bytes, not frames: no arm can name that phase, so
/// every kind is refused there whatever the table says.
#[test]
fn no_frame_of_any_kind_survives_the_preface() {
    let mut kinds = 0;
    for raw in 0..u16::MAX {
        let Some(kind) = MessageKind::from_u16(raw) else {
            continue;
        };
        kinds += 1;
        let mut driver = ConnectionDriver::daemon();
        let err = driver
            .classify(&OwnedFrame {
                kind: raw,
                body: Bytes::new(),
            })
            .unwrap_err();
        assert!(
            matches!(&err, DriverError::UnexpectedKind { expected, .. } if expected.contains("preface")),
            "{kind} got {err}"
        );
    }
    assert_eq!(kinds, 10, "every frame kind is covered");
}

#[test]
fn each_phase_admits_only_its_own_frames() {
    let mut driver = ConnectionDriver::daemon();
    assert!(matches!(
        driver
            .classify(&frame(&ConnToDaemonMsg::Hello {
                mode: ConnectionMode::Ops,
                pull_paced: false
            }))
            .unwrap_err(),
        DriverError::UnexpectedKind { .. }
    ));

    driver.preface_done();
    let err = driver
        .classify(&OwnedFrame {
            kind: MessageKind::Ops.as_u16(),
            body: Bytes::new(),
        })
        .unwrap_err();
    assert!(
        matches!(&err, DriverError::OutOfPhase { arm, phase: Phase::Handshake, .. }
            if *arm == "Ops"),
        "got {err}"
    );

    assert!(matches!(
        driver
            .classify(&frame(&ConnToDaemonMsg::Hello {
                mode: ConnectionMode::Ops,
                pull_paced: false
            }))
            .unwrap(),
        Incoming::Control(ConnToDaemonMsg::Hello { .. })
    ));
    driver.handshake_done(ConnectionMode::Ops);
    assert_eq!(driver.phase(), Phase::Setup);
}

#[test]
fn a_second_hello_after_the_handshake_closes_the_connection() {
    for phase in [Phase::Setup, Phase::Attached] {
        let mut driver = in_phase::<DaemonSide>(phase, ConnectionMode::Ops);
        let err = driver
            .classify(&frame(&ConnToDaemonMsg::Hello {
                mode: ConnectionMode::Ops,
                pull_paced: false,
            }))
            .unwrap_err();
        assert!(
            matches!(&err, DriverError::OutOfPhase { arm, legal, .. }
                if *arm == "Conn::Hello" && legal == "handshake"),
            "got {err}"
        );
    }
}

// ── mode admission ──────────────────────────────────────────────────

#[test]
fn a_mode_refuses_a_surface_it_never_asked_for() {
    let mut observer = in_phase::<DaemonSide>(Phase::Setup, ConnectionMode::Observer);
    let err = observer
        .classify(&frame(&SearchToDaemonMsg::Query {
            query: "x".into(),
            options: SearchOptions::default(),
        }))
        .unwrap_err();
    assert!(
        matches!(
            &err,
            DriverError::ModeDenied {
                mode: ConnectionMode::Observer,
                kind: MessageKind::Search
            }
        ),
        "got {err}"
    );

    let mut window = in_phase::<DaemonSide>(Phase::Setup, ConnectionMode::Window);
    assert!(matches!(
        window
            .classify(&frame(&NotifyToDaemonMsg::Subscribe {
                session_prefix: None
            }))
            .unwrap_err(),
        DriverError::ModeDenied { .. }
    ));
}

/// The kind gate is the fold of the arms, so it admits `Ops` to a
/// `Window`; the arm gate after the decode is what separates the pure
/// queries from the mutations.
#[test]
fn a_window_may_query_the_roster_but_not_mutate_it() {
    let mut daemon = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Window);

    for (nth, (verb, admitted)) in [
        (OpsToDaemonMsg::List, true),
        (OpsToDaemonMsg::Status, true),
        (
            OpsToDaemonMsg::Destroy {
                id_prefix: "ab".into(),
            },
            false,
        ),
        (
            OpsToDaemonMsg::Tag {
                id_prefix: "ab".into(),
                add: vec!["build".into()],
                remove: vec![],
            },
            false,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        // The daemon holds the request sequence, so even the verbs it
        // refuses have to arrive on their own id.
        let request = Correlation::request(
            RequestId::new(u64::try_from(nth).expect("small") + 1).expect("nonzero"),
        );
        let Incoming::Payload(payload) = daemon.classify(&correlated(&verb, request)).unwrap()
        else {
            panic!("an Ops verb is a payload");
        };
        let outcome = daemon.decode::<OpsToDaemonMsg>(&payload);
        if admitted {
            assert!(
                matches!(outcome, Ok(Delivery::Deliver(_))),
                "{verb:?} is a query every attach-capable mode may run"
            );
        } else {
            assert!(
                matches!(
                    outcome,
                    Err(DriverError::ArmDenied {
                        mode: ConnectionMode::Window,
                        ..
                    })
                ),
                "{verb:?} must be refused on a Window connection"
            );
        }
    }
}

/// The mode column is receive-side too: the window-management pushes
/// ask a *window* to move, which a scripted `Ops` attach has none to
/// do, while an observer, which attached to nothing, takes none of them.
#[test]
fn an_ops_client_refuses_a_window_management_push() {
    let mut ops = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Ops);
    let Incoming::Payload(evicted) = ops
        .classify(&frame(&PushMsg::Evicted {
            reason: "evicted".into(),
        }))
        .unwrap()
    else {
        panic!("a push is a payload");
    };
    assert!(
        matches!(
            ops.decode::<PushMsg>(&evicted),
            Ok(Delivery::Deliver(Delivered {
                msg: PushMsg::Evicted { .. },
                ..
            }))
        ),
        "an Ops attach is a real subscriber, so eviction concerns it"
    );

    for push in [
        PushMsg::Reattach { id: 1 },
        PushMsg::SessionExited { id: 1 },
    ] {
        let Incoming::Payload(payload) = ops.classify(&frame(&push)).unwrap() else {
            panic!("a push is a payload");
        };
        assert!(
            matches!(
                ops.decode::<PushMsg>(&payload),
                Err(DriverError::ArmDenied {
                    mode: ConnectionMode::Ops,
                    ..
                })
            ),
            "{push:?} is a Window-only arm"
        );
    }

    let mut window = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let Incoming::Payload(payload) = window
        .classify(&frame(&PushMsg::Reattach { id: 1 }))
        .unwrap()
    else {
        panic!("a push is a payload");
    };
    assert!(matches!(
        window.decode::<PushMsg>(&payload),
        Ok(Delivery::Deliver(Delivered {
            msg: PushMsg::Reattach { .. },
            ..
        }))
    ));

    let mut observer = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Observer);
    for push in [
        PushMsg::Evicted {
            reason: "evicted".into(),
        },
        PushMsg::Reattach { id: 1 },
    ] {
        assert!(
            matches!(
                observer.classify(&frame(&push)),
                Err(DriverError::ModeDenied {
                    mode: ConnectionMode::Observer,
                    ..
                })
            ),
            "an observer attaches to nothing, so {push:?} is not its traffic"
        );
    }
}

#[test]
fn grid_pushes_reach_a_window_uncorrelated() {
    let mut driver = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    for msg in [
        GridMsg::RehydrateBegin,
        GridMsg::RowDelta { rows: vec![] },
        GridMsg::RehydrateEnd,
    ] {
        let Incoming::Payload(p) = driver.classify(&frame(&msg)).unwrap() else {
            panic!("payload");
        };
        assert_eq!(p.correlation, None);
        assert!(matches!(
            driver.decode::<GridMsg>(&p).unwrap(),
            Delivery::Deliver(_)
        ));
    }
}

/// An envelope that does not match the arm's class names no
/// conversation, and REQ-114 answers an unattributable frame by ending
/// the connection: a fabricated `Region::Reply` must not reach a window
/// that never asked for a region.
#[test]
fn an_envelope_that_contradicts_the_arms_correlation_class_is_refused() {
    let a_reply = RegionToClientMsg::Reply {
        data: b"row".to_vec(),
        position: None,
        exit_code: None,
    };
    let a_row = RegionToClientMsg::Row {
        row: 0,
        text: "row".into(),
        ansi: None,
        soft_wrap_continued: false,
    };
    let request = Correlation::request(RequestId::new(1).expect("nonzero"));

    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    for (frame, why) in [
        (
            frame(&a_reply),
            "a reply with no request_id answers no request",
        ),
        (
            correlated(&a_reply, Correlation::stream(stream(1))),
            "a reply is not a stream item",
        ),
        (correlated(&a_row, request), "a stream item is not a reply"),
    ] {
        let Incoming::Payload(payload) = client.classify(&frame).unwrap() else {
            panic!("a region frame is a payload");
        };
        assert!(
            matches!(
                client.decode::<RegionToClientMsg>(&payload),
                Err(DriverError::Correlation { .. })
            ),
            "{why}"
        );
    }

    // `Session` carries the envelope slot and never fills it, and its
    // uncorrelated arms are held to that.
    let mut daemon = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Window);
    let Incoming::Payload(payload) = daemon
        .classify(&correlated(&SessionToDaemonMsg::Detach, request))
        .unwrap()
    else {
        panic!("a session frame is a payload");
    };
    assert!(matches!(
        daemon.decode::<SessionToDaemonMsg>(&payload),
        Err(DriverError::Correlation { .. })
    ));

    let Incoming::Payload(payload) = daemon.classify(&frame(&OpsToDaemonMsg::List)).unwrap() else {
        panic!("an ops frame is a payload");
    };
    assert!(
        matches!(
            daemon.decode::<OpsToDaemonMsg>(&payload),
            Err(DriverError::Correlation { .. })
        ),
        "a request with no request_id can never be answered"
    );
}

/// REQ-114: a frame the receiver has no use for passes the arm table
/// before it is dropped, the correlation column included.
#[test]
fn a_drained_frame_whose_envelope_contradicts_its_arm_is_fatal() {
    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let mislabeled = correlated(
        &a_match(),
        Correlation::request(RequestId::new(1).expect("nonzero")),
    );
    let Incoming::Payload(p) = client.classify(&mislabeled).unwrap() else {
        panic!("a search match is a payload");
    };
    assert!(matches!(
        client.admit_drained(&p),
        Err(DriverError::Correlation { .. })
    ));
}

/// Draining accounts: the item names a stream this client never
/// opened, and only the stream table can say so, so the drain is where
/// it is caught rather than a frame quietly thrown away.
#[test]
fn a_drained_item_for_a_stream_the_client_never_opened_is_fatal() {
    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let item = correlated(&a_match(), Correlation::stream(stream(7)));
    let Incoming::Payload(p) = client.classify(&item).unwrap() else {
        panic!("a search match is a payload");
    };
    assert!(matches!(
        client.admit_drained(&p),
        Err(DriverError::Correlation { .. })
    ));
    assert_eq!(client.outstanding_streams(), 0);
}

/// A frame held back until a verb's reply arrives gets the same
/// judgment as one that is dropped, and nothing stateful with it: the
/// requeued frame is read again.
#[test]
fn a_parked_frame_is_judged_by_the_arm_table_without_being_accounted() {
    let ops = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Ops);
    assert!(matches!(
        ops.admit_parked(&frame(&PushMsg::Reattach { id: 1 })),
        Err(DriverError::ArmDenied {
            mode: ConnectionMode::Ops,
            ..
        })
    ));

    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let item = correlated(&a_match(), Correlation::stream(stream(7)));
    assert!(client.admit_parked(&item).is_ok());
    assert_eq!(client.outstanding_streams(), 0);
    // The requeued frame is still the one the stream table sees.
    let Incoming::Payload(p) = client.classify(&item).unwrap() else {
        panic!("a search match is a payload");
    };
    assert!(matches!(
        client.decode::<SearchToClientMsg>(&p),
        Err(DriverError::Correlation { .. })
    ));
}

// ── request/reply ───────────────────────────────────────────────────

/// The requests were issued in order but the replies may not be: a
/// slow verb must not hold up the answer to a later one.
#[test]
fn replies_match_their_requests_in_any_order() {
    let mut driver = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Ops);
    let first = driver.issue_request().expect("the sequence is fresh");
    let second = driver.issue_request().expect("the sequence is fresh");

    for id in [second, first] {
        let f = correlated(
            &OpsToClientMsg::Listed { sessions: vec![] },
            Correlation::request(id),
        );
        let Incoming::Payload(p) = driver.classify(&f).unwrap() else {
            panic!("payload");
        };
        let Delivery::Deliver(delivered) = driver.decode::<OpsToClientMsg>(&p).unwrap() else {
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
    let listed = correlated(
        &OpsToClientMsg::Listed { sessions: vec![] },
        Correlation::request(id),
    );

    let Incoming::Payload(p) = driver.classify(&listed).unwrap() else {
        panic!("payload");
    };
    assert!(driver.decode::<OpsToClientMsg>(&p).is_ok());

    let Incoming::Payload(p) = driver.classify(&listed).unwrap() else {
        panic!("payload");
    };
    let err = driver.decode::<OpsToClientMsg>(&p).unwrap_err();
    assert!(
        matches!(&err, DriverError::Correlation { found, .. } if found.contains("not outstanding")),
        "got {err}"
    );

    let never = correlated(
        &OpsToClientMsg::Listed { sessions: vec![] },
        Correlation::request(RequestId::new(9).expect("nonzero")),
    );
    let Incoming::Payload(p) = driver.classify(&never).unwrap() else {
        panic!("payload");
    };
    assert!(matches!(
        driver.decode::<OpsToClientMsg>(&p).unwrap_err(),
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
    let Incoming::Payload(p) = driver.classify(&f).unwrap() else {
        panic!("payload");
    };
    let err = driver.decode::<OpsToClientMsg>(&p).unwrap_err();
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
    let f = correlated(&OpsToDaemonMsg::List, Correlation::stream(stream(1)));
    let Incoming::Payload(p) = driver.classify(&f).unwrap() else {
        panic!("payload");
    };
    let err = driver.decode::<OpsToDaemonMsg>(&p).unwrap_err();
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

    let f = correlated(&a_query(), Correlation::stream(stream(3)));
    let Incoming::Payload(p) = driver.classify(&f).unwrap() else {
        panic!("payload");
    };
    let Delivery::RefuseStream { reply } = driver.decode::<SearchToDaemonMsg>(&p).unwrap() else {
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

// ── correlation class ───────────────────────────────────────────────

/// Decode a payload as this side's wrapper of the family its arm
/// belongs to, discarding the delivery: the table below only asks
/// whether the driver refused.
trait DecodeAsItsFamily {
    fn decode_family(&mut self, p: &Payload) -> Result<(), DriverError>;
}

impl DecodeAsItsFamily for DaemonDriver {
    fn decode_family(&mut self, p: &Payload) -> Result<(), DriverError> {
        match p.kind {
            MessageKind::Ops => self.decode::<OpsToDaemonMsg>(p).map(drop),
            MessageKind::Region => self.decode::<RegionToDaemonMsg>(p).map(drop),
            MessageKind::Search => self.decode::<SearchToDaemonMsg>(p).map(drop),
            MessageKind::Notify => self.decode::<NotifyToDaemonMsg>(p).map(drop),
            MessageKind::Session => self.decode::<SessionToDaemonMsg>(p).map(drop),
            other => panic!("no {other} case in the table"),
        }
    }
}

impl DecodeAsItsFamily for ClientDriver {
    fn decode_family(&mut self, p: &Payload) -> Result<(), DriverError> {
        match p.kind {
            MessageKind::Ops => self.decode::<OpsToClientMsg>(p).map(drop),
            MessageKind::Region => self.decode::<RegionToClientMsg>(p).map(drop),
            MessageKind::Search => self.decode::<SearchToClientMsg>(p).map(drop),
            MessageKind::Notify => self.decode::<NotifyToClientMsg>(p).map(drop),
            MessageKind::Session => self.decode::<SessionToClientMsg>(p).map(drop),
            other => panic!("no {other} case in the table"),
        }
    }
}

fn a_region_reply() -> RegionToClientMsg {
    RegionToClientMsg::Reply {
        data: Vec::new(),
        position: None,
        exit_code: None,
    }
}

fn a_region_rows() -> RegionToDaemonMsg {
    RegionToDaemonMsg::Rows {
        source: RegionSource::Scrollback,
        ansi: false,
        max_rows: None,
    }
}

fn a_region_row() -> RegionToClientMsg {
    RegionToClientMsg::Row {
        row: 0,
        text: "hit".into(),
        ansi: None,
        soft_wrap_continued: false,
    }
}

/// A driver primed so the envelope under test names an id that would
/// pass the accounting if the class allowed it: request 1 outstanding
/// and stream 1 live on the client, the next unissued ids on the
/// daemon. What refuses the frame is then the class alone.
fn primed<R: Role>(phase: Phase, mode: ConnectionMode) -> ConnectionDriver<R> {
    let mut driver = in_phase::<R>(phase, mode);
    if R::SIDE == Side::Client {
        let _issued = driver.issue_request().expect("the sequence is fresh");
        let _opened = driver.open_stream().expect("the sequence is fresh");
    }
    driver
}

/// Wrong identity *class* on every conversation family and every class
/// an arm can declare: an envelope of the other kind makes the frame
/// unroutable however plausible its id looks.
fn refused<R: Role>(what: &str, mut driver: ConnectionDriver<R>, f: &OwnedFrame) -> DriverError
where
    ConnectionDriver<R>: DecodeAsItsFamily,
{
    let Incoming::Payload(p) = driver.classify(f).unwrap() else {
        panic!("{what}: a family frame is a payload");
    };
    driver
        .decode_family(&p)
        .expect_err(&format!("{what}: the wrong envelope must be refused"))
}

#[test]
fn an_envelope_of_the_wrong_class_closes_the_connection_on_every_family() {
    let request_envelope = || Correlation::request(request(1));
    let stream_envelope = || Correlation::stream(stream(1));
    // Arm, the side that receives it, the mode and phase it arrives
    // in, the envelope it must not carry, and the family to decode it
    // as.
    let cases: Vec<(&str, Side, ConnectionMode, Phase, OwnedFrame)> = vec![
        (
            "Session::Detach is uncorrelated",
            Side::Daemon,
            ConnectionMode::Window,
            Phase::Attached,
            correlated(&SessionToDaemonMsg::Detach, request_envelope()),
        ),
        (
            "Ops::List opens a request",
            Side::Daemon,
            ConnectionMode::Ops,
            Phase::Attached,
            correlated(&OpsToDaemonMsg::List, stream_envelope()),
        ),
        (
            "Ops::Listed answers one",
            Side::Client,
            ConnectionMode::Ops,
            Phase::Attached,
            correlated(
                &OpsToClientMsg::Listed {
                    sessions: Vec::new(),
                },
                stream_envelope(),
            ),
        ),
        (
            "Region::Request opens a request",
            Side::Daemon,
            ConnectionMode::Window,
            Phase::Attached,
            correlated(
                &RegionToDaemonMsg::Request {
                    source: RegionSource::Scrollback,
                    ansi: false,
                },
                stream_envelope(),
            ),
        ),
        (
            "Region::Reply answers one",
            Side::Client,
            ConnectionMode::Window,
            Phase::Attached,
            correlated(&a_region_reply(), stream_envelope()),
        ),
        (
            "Region::Rows opens a stream",
            Side::Daemon,
            ConnectionMode::Window,
            Phase::Attached,
            correlated(&a_region_rows(), request_envelope()),
        ),
        (
            "Region::Row is a stream item",
            Side::Client,
            ConnectionMode::Window,
            Phase::Attached,
            correlated(&a_region_row(), request_envelope()),
        ),
        (
            "Region::RowsDone is a stream item",
            Side::Client,
            ConnectionMode::Window,
            Phase::Attached,
            correlated(
                &RegionToClientMsg::RowsDone { exit_code: None },
                request_envelope(),
            ),
        ),
        (
            "Search::Query opens a stream",
            Side::Daemon,
            ConnectionMode::Ops,
            Phase::Attached,
            correlated(&a_query(), request_envelope()),
        ),
        (
            "Search::Match is a stream item",
            Side::Client,
            ConnectionMode::Ops,
            Phase::Attached,
            correlated(&a_match(), request_envelope()),
        ),
        (
            "Notify::Subscribe opens a stream",
            Side::Daemon,
            ConnectionMode::Observer,
            Phase::Setup,
            correlated(
                &NotifyToDaemonMsg::Subscribe {
                    session_prefix: None,
                },
                request_envelope(),
            ),
        ),
        (
            "Notify::Lagged is a stream item",
            Side::Client,
            ConnectionMode::Observer,
            Phase::Observing,
            correlated(&NotifyToClientMsg::Lagged { missed: 1 }, request_envelope()),
        ),
    ];

    for (what, side, mode, phase, f) in cases {
        let err = match side {
            Side::Daemon => refused(what, primed::<DaemonSide>(phase, mode), &f),
            Side::Client => refused(what, primed::<ClientSide>(phase, mode), &f),
        };
        assert!(
            matches!(err, DriverError::Correlation { .. }),
            "{what}: got {err}"
        );
    }
}

/// The refusal names the arm and takes no stream slot: a class the
/// driver rejected must not leave accounting behind for a stream
/// nothing will ever terminate.
#[test]
fn an_envelope_of_the_wrong_class_names_the_arm_and_frees_no_slot() {
    // A request opener carrying a stream id.
    let mut daemon = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
    let f = correlated(&OpsToDaemonMsg::List, Correlation::stream(stream(1)));
    let Incoming::Payload(p) = daemon.classify(&f).unwrap() else {
        panic!("payload");
    };
    let err = daemon.decode::<OpsToDaemonMsg>(&p).unwrap_err();
    assert!(
        matches!(&err, DriverError::Correlation { expected, found, .. }
            if expected.contains("next unissued request") && found.contains("Ops::List")),
        "got {err}"
    );
    // A slot taken by an arm that never terminates a stream is a slot
    // nothing frees.
    assert_eq!(daemon.outstanding_streams(), 0);

    // An uncorrelated arm of a family whose wrapper carries the slot.
    let mut daemon = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Window);
    let f = correlated(
        &SessionToDaemonMsg::Detach,
        Correlation::request(request(1)),
    );
    let Incoming::Payload(p) = daemon.classify(&f).unwrap() else {
        panic!("payload");
    };
    let err = daemon.decode::<SessionToDaemonMsg>(&p).unwrap_err();
    assert!(
        matches!(&err, DriverError::Correlation { expected, .. }
            if expected.contains("no correlation envelope")),
        "got {err}"
    );
}

/// The encoder refuses a pairing in the words the receiving driver
/// uses for it.
#[test]
fn the_encoder_and_the_driver_word_one_mismatch_identically() {
    let envelope = Correlation::stream(stream(1));
    let sender = crate::framing::CheckedFrame::encode_correlated(&OpsToDaemonMsg::List, envelope)
        .expect_err("a stream id on a request opener");

    let mut daemon = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
    let f = correlated(&OpsToDaemonMsg::List, envelope);
    let Incoming::Payload(p) = daemon.classify(&f).unwrap() else {
        panic!("payload");
    };
    let receiver = daemon.decode::<OpsToDaemonMsg>(&p).unwrap_err();

    assert_eq!(sender.to_string(), receiver.to_string());
}

/// Field 100 stamped onto a body by hand. The `Correlated` trait only
/// exists on the families whose wrapper declares the slot, and a peer
/// that ignores that declaration is exactly what this covers.
fn with_an_envelope<M: WireCodec>(msg: &M) -> OwnedFrame {
    let mut body = codec::encode(msg);
    // Tag 100, wire type 2, wrapping a `Correlation` whose request_id
    // is 1.
    body.extend_from_slice(&[0xA2, 0x06, 0x02, 0x08, 0x01]);
    OwnedFrame {
        kind: M::KIND.as_u16(),
        body: Bytes::from(body),
    }
}

/// The `uncorrelated` class is checked on the families whose wrapper
/// reserves field 100 as much as on the ones that use it: a reserved
/// field prost would skip still reads back as an envelope no arm
/// claims, which is unattributable correlation and ends the
/// connection (REQ-114).
#[test]
fn an_envelope_on_a_family_that_declares_none_closes_the_connection() {
    use felis_protocol::ImageId;

    let mut daemon = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Window);
    let f = with_an_envelope(&InputMsg::FocusChange { focused: true });
    let Incoming::Payload(p) = daemon.classify(&f).unwrap() else {
        panic!("input is a payload");
    };
    let err = daemon.decode::<InputMsg>(&p).unwrap_err();
    assert!(
        matches!(&err, DriverError::Correlation { expected, .. }
            if expected.contains("no correlation envelope")),
        "got {err}"
    );

    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let f = with_an_envelope(&GridMsg::RehydrateBegin);
    let Incoming::Payload(p) = client.classify(&f).unwrap() else {
        panic!("a grid delta is a payload");
    };
    assert!(matches!(
        client.decode::<GridMsg>(&p).unwrap_err(),
        DriverError::Correlation { .. }
    ));

    let f = with_an_envelope(&ImageMsg::Delete { id: ImageId(7) });
    let Incoming::Payload(p) = client.classify(&f).unwrap() else {
        panic!("an image event is a payload");
    };
    assert!(matches!(
        client.decode::<ImageMsg>(&p).unwrap_err(),
        DriverError::Correlation { .. }
    ));

    let f = with_an_envelope(&PushMsg::Evicted {
        reason: "evicted".into(),
    });
    let Incoming::Payload(p) = client.classify(&f).unwrap() else {
        panic!("a push is a payload");
    };
    assert!(matches!(
        client.decode::<PushMsg>(&p).unwrap_err(),
        DriverError::Correlation { .. }
    ));

    // `Conn` never becomes a payload, so its check runs in `classify`.
    let live = client.open_stream().expect("the sequence is fresh");
    let err = client
        .classify(&with_an_envelope(&ConnToClientMsg::End {
            stream_id: live,
            count: 0,
        }))
        .unwrap_err();
    assert!(
        matches!(&err, DriverError::Correlation { expected, .. }
            if expected.contains("no correlation envelope")),
        "got {err}"
    );
    assert_eq!(
        client.classify_stream(live),
        StreamClass::Active,
        "a refused terminal closed nothing"
    );
}

/// Field 100 holding a hand-written sequence of `(tag, value)` pairs.
/// Hand-written because the generated type has no way to write a tag
/// twice, which is what a peer naming both ids does.
fn with_hand_written_ids<M: WireCodec>(msg: &M, fields: &[(u8, u8)]) -> OwnedFrame {
    let mut slot = Vec::new();
    for &(tag, value) in fields {
        slot.push(tag << 3);
        slot.push(value);
    }
    let mut body = codec::encode(msg);
    body.extend_from_slice(&[0xA2, 0x06]);
    body.push(u8::try_from(slot.len()).expect("a hand-written slot stays short"));
    body.extend_from_slice(&slot);
    OwnedFrame {
        kind: M::KIND.as_u16(),
        body: Bytes::from(body),
    }
}

/// A body naming both ids is refused whatever order the peer wrote the
/// tags in. The frame never becomes a delivery: a body a canonical
/// reader resolves to a zero `request_id` must not reach the daemon as
/// a stream opener.
#[test]
fn a_both_ids_envelope_closes_the_connection_in_every_order() {
    for fields in [
        &[(1, 1), (2, 1)][..],
        &[(2, 1), (1, 1)][..],
        &[(1, 7), (2, 1), (1, 0)][..],
        &[(2, 1), (1, 0)][..],
        &[(1, 0), (2, 1)][..],
    ] {
        let mut daemon = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
        let err = daemon
            .classify(&with_hand_written_ids(&OpsToDaemonMsg::List, fields))
            .unwrap_err();
        assert!(
            matches!(&err, DriverError::Correlation { expected, .. }
                if expected.contains("well-formed correlation envelope")),
            "{fields:?} gave {err}"
        );
    }
}

/// A reader that discards a family still owes it the table. Every
/// client drains traffic it has no use for (the CLI bridge drops the
/// grid burst, a one-shot verb drops another family's frame), and
/// matching on the kind to skip it would exempt exactly those frames
/// from the checks `decode` is the only place to run.
#[test]
fn a_drained_family_is_still_admitted_through_the_arm_table() {
    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);

    let honest = frame(&GridMsg::RehydrateBegin);
    let Incoming::Payload(p) = client.classify(&honest).unwrap() else {
        panic!("a grid push is a payload");
    };
    client
        .admit_drained(&p)
        .expect("an uncorrelated push is what this family sends");

    let stamped = with_an_envelope(&GridMsg::RehydrateBegin);
    let Incoming::Payload(p) = client.classify(&stamped).unwrap() else {
        panic!("a grid push is a payload");
    };
    assert!(
        matches!(
            client.admit_drained(&p).unwrap_err(),
            DriverError::Correlation { .. }
        ),
        "an envelope no Grid arm claims is unattributable, drained or not"
    );

    let malformed = with_a_malformed_envelope(&GridMsg::RehydrateBegin);
    let Incoming::Payload(p) = client.classify(&malformed).unwrap() else {
        panic!("a grid push is a payload");
    };
    assert!(
        matches!(
            client.admit_drained(&p).unwrap_err(),
            DriverError::Correlation { .. }
        ),
        "the held peek fault is re-raised for a drained family too"
    );
}

/// The kind columns hold on the drain path too. `classify` refuses
/// this frame first on a live connection, so this is what keeps the
/// arm-table-free drain of a uniform family from being the one way
/// into the driver that skips the table.
#[test]
fn a_drained_family_the_mode_may_not_see_is_refused() {
    let mut observer = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Observer);
    let payload = Payload {
        kind: MessageKind::Grid,
        body: Bytes::from(codec::encode(&GridMsg::RehydrateBegin)),
        correlation: None,
        envelope_fault: None,
    };
    assert!(
        matches!(
            observer.admit_drained(&payload).unwrap_err(),
            DriverError::ModeDenied {
                kind: MessageKind::Grid,
                ..
            }
        ),
        "an observer never subscribes to a grid"
    );
}

/// A drained frame as the reader receives it: through `classify`, so
/// the envelope peek has run over the body the test wrote.
fn drained<R: Role>(driver: &mut ConnectionDriver<R>, kind: MessageKind, body: Vec<u8>) -> Payload {
    let frame = OwnedFrame {
        kind: kind.as_u16(),
        body: Bytes::from(body),
    };
    let Incoming::Payload(payload) = driver.classify(&frame).unwrap() else {
        panic!("an uncorrelated push is a payload")
    };
    payload
}

/// The uniform families skip the arm-table walk on the drain path, not
/// the decode: a body that is not protobuf is corruption for the role
/// that drops grid traffic exactly as it is for the window client that
/// draws it. The peek already failed on these bytes, so this also
/// pins which of the two faults is reported.
#[test]
fn a_drained_uniform_body_that_is_not_protobuf_is_corruption() {
    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);

    let honest = drained(
        &mut client,
        MessageKind::Grid,
        codec::encode(&GridMsg::RehydrateBegin),
    );
    client
        .admit_drained(&honest)
        .expect("a well-formed grid push is admitted");

    // A tag varint that never terminates.
    let garbage = drained(&mut client, MessageKind::Grid, vec![0xFF, 0xFF, 0xFF]);
    assert!(
        matches!(
            client.admit_drained(&garbage).unwrap_err(),
            DriverError::UndecodableBody {
                expected: MessageKind::Grid,
                ..
            }
        ),
        "a drained body that is not protobuf is corruption, not a correlation fault"
    );
}

/// An arm outside the schema is skipped by prost as an unknown field
/// and leaves the family's oneof absent, which the conversion refuses.
#[test]
fn a_drained_uniform_body_naming_no_known_arm_is_corruption() {
    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    // Field 4095, wire type 2, zero-length: no `GridMsg` arm claims it.
    let unknown_arm = drained(&mut client, MessageKind::Grid, vec![0xFA, 0xFF, 0x01, 0x00]);
    let err = client.admit_drained(&unknown_arm).unwrap_err();
    assert!(
        matches!(
            &err,
            DriverError::UndecodableBody { expected: MessageKind::Grid, found }
                if found.contains("GridMsg.msg")
        ),
        "an unknown arm left the oneof absent, which is corruption: {err}"
    );
}

/// The stateless limits `felis-protocol` can check run on the drain
/// path too: an image header claiming more pixels than the per-image
/// cap is an allocation a consumer would have to refuse anyway.
#[test]
fn a_drained_uniform_body_over_a_stateless_limit_is_corruption() {
    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let oversized = ImageMsg::Header {
        id: ImageId(1),
        target: ImageTarget::New {
            width: u32::MAX,
            height: u32::MAX,
            format: ImageFormat::Rgba32,
        },
    };
    let payload = drained(&mut client, MessageKind::Image, codec::encode(&oversized));
    let err = client.admit_drained(&payload).unwrap_err();
    assert!(
        matches!(
            &err,
            DriverError::UndecodableBody { expected: MessageKind::Image, found }
                if found.contains("ImageNew.pixels")
        ),
        "an over-cap image claim is corruption on the drain path: {err}"
    );
}

/// The boundary the drain path cannot cross. `packed_cells` is opaque
/// to `felis-protocol`, so a row holding bytes no row codec produced
/// is a well-formed frame at the wire tier; only the shadow that
/// unpacks it, against the row codec in force, can call it malformed
/// (`docs/reference/ipc.md` "Corruption").
#[test]
fn a_drained_uniform_body_with_an_opaque_garbage_row_is_admitted() {
    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let garbage_row = GridMsg::RowDelta {
        rows: vec![(0, RowPayload(vec![0xFF; 16]))],
    };
    let payload = drained(&mut client, MessageKind::Grid, codec::encode(&garbage_row));
    client
        .admit_drained(&payload)
        .expect("row-codec contents are a consumer-tier invariant, not a wire-tier one");
}

/// Field 100 present but holding bytes that are not a `Correlation`.
/// The nested length is honest, so prost skips the whole field as an
/// unknown one and the family decode succeeds: only the peek sees the
/// fault, and only a family whose wrapper reserves the slot can reach
/// this state (one that declares it fails its own decode instead).
fn with_a_malformed_envelope<M: WireCodec>(msg: &M) -> OwnedFrame {
    let mut body = codec::encode(msg);
    // Tag 100, wire type 2, one byte of payload: 0xFF, a truncated
    // varint that is not a field key.
    body.extend_from_slice(&[0xA2, 0x06, 0x01, 0xFF]);
    OwnedFrame {
        kind: M::KIND.as_u16(),
        body: Bytes::from(body),
    }
}

/// A malformed envelope is malformed on every family (REQ-113,
/// REQ-114), including the uncorrelated ones. The fault is raised only
/// once the family decode has succeeded, which is what proves the
/// envelope alone was what failed.
#[test]
fn a_malformed_envelope_closes_the_connection_on_an_uncorrelated_family() {
    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let f = with_a_malformed_envelope(&GridMsg::RehydrateBegin);
    let Incoming::Payload(p) = client.classify(&f).unwrap() else {
        panic!("a grid push is a payload");
    };
    assert_eq!(p.correlation, None, "the peek recovered no identity");
    let err = client.decode::<GridMsg>(&p).unwrap_err();
    assert!(
        matches!(&err, DriverError::Correlation { expected, .. }
            if expected.contains("well-formed correlation envelope")),
        "got {err}"
    );

    // `Conn` never becomes a payload, so its check runs in `classify`.
    let live = client.open_stream().expect("the sequence is fresh");
    let err = client
        .classify(&with_a_malformed_envelope(&ConnToClientMsg::End {
            stream_id: live,
            count: 0,
        }))
        .unwrap_err();
    assert!(
        matches!(&err, DriverError::Correlation { expected, .. }
            if expected.contains("well-formed correlation envelope")),
        "got {err}"
    );
    assert_eq!(
        client.classify_stream(live),
        StreamClass::Active,
        "a refused terminal closed nothing"
    );
}

/// The counterpart: a body that is not protobuf at all fails the same
/// peek, and naming the envelope for it would report a correlation
/// fault for every corrupt frame.
#[test]
fn a_body_that_is_not_protobuf_is_an_undecodable_body_not_a_correlation_fault() {
    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let f = OwnedFrame {
        kind: MessageKind::Grid.as_u16(),
        body: Bytes::from_static(&[0xFF, 0xFF, 0xFF]),
    };
    let Incoming::Payload(p) = client.classify(&f).unwrap() else {
        panic!("a grid push is a payload");
    };
    assert!(matches!(
        client.decode::<GridMsg>(&p).unwrap_err(),
        DriverError::UndecodableBody { .. }
    ));
}

/// The class also makes the envelope mandatory: an opener whose id is
/// missing is refused at the decode, so no handler downstream has an
/// id to recover.
#[test]
fn a_missing_envelope_closes_the_connection_on_every_correlated_class() {
    let mut daemon = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
    let f = frame(&OpsToDaemonMsg::List);
    let Incoming::Payload(p) = daemon.classify(&f).unwrap() else {
        panic!("payload");
    };
    assert!(matches!(
        daemon.decode::<OpsToDaemonMsg>(&p).unwrap_err(),
        DriverError::Correlation { .. }
    ));

    let f = frame(&a_query());
    let Incoming::Payload(p) = daemon.classify(&f).unwrap() else {
        panic!("payload");
    };
    assert!(matches!(
        daemon.decode::<SearchToDaemonMsg>(&p).unwrap_err(),
        DriverError::Correlation { .. }
    ));

    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let f = frame(&a_match());
    let Incoming::Payload(p) = client.classify(&f).unwrap() else {
        panic!("payload");
    };
    assert!(matches!(
        client.decode::<SearchToClientMsg>(&p).unwrap_err(),
        DriverError::Correlation { .. }
    ));
}

/// The daemon holds the same request counter the client allocates
/// from, so a gap is caught at the request rather than at whatever the
/// reply happens to answer.
#[test]
fn a_skipped_request_id_closes_the_connection() {
    let mut daemon = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
    for id in [1, 3] {
        let f = correlated(&OpsToDaemonMsg::List, Correlation::request(request(id)));
        let Incoming::Payload(p) = daemon.classify(&f).unwrap() else {
            panic!("payload");
        };
        let outcome = daemon.decode::<OpsToDaemonMsg>(&p);
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
    let f = correlated(&OpsToDaemonMsg::List, Correlation::request(request(1)));
    for expect_ok in [true, false] {
        let Incoming::Payload(p) = daemon.classify(&f).unwrap() else {
            panic!("payload");
        };
        assert_eq!(daemon.decode::<OpsToDaemonMsg>(&p).is_ok(), expect_ok);
    }
}

/// The two sequences advance independently: opening streams must not
/// consume request ids, and vice versa.
#[test]
fn the_request_and_stream_sequences_stay_independent_at_the_daemon() {
    let mut daemon = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
    open_on_daemon(&mut daemon, stream(1));
    let f = correlated(&OpsToDaemonMsg::List, Correlation::request(request(1)));
    let Incoming::Payload(p) = daemon.classify(&f).unwrap() else {
        panic!("payload");
    };
    assert!(matches!(
        daemon.decode::<OpsToDaemonMsg>(&p).unwrap(),
        Delivery::Deliver(_)
    ));
    open_on_daemon(&mut daemon, stream(2));
}

/// The identity the driver validated travels with the message, so a
/// handler reads it back instead of re-deriving it from the envelope.
#[test]
fn a_delivery_carries_the_identity_its_class_declares() {
    let mut daemon = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
    let f = correlated(&OpsToDaemonMsg::List, Correlation::request(request(1)));
    let Incoming::Payload(p) = daemon.classify(&f).unwrap() else {
        panic!("payload");
    };
    let Delivery::Deliver(delivered) = daemon.decode::<OpsToDaemonMsg>(&p).unwrap() else {
        panic!("delivered");
    };
    assert_eq!(delivered.request(), request(1));

    let f = correlated(&a_query(), Correlation::stream(stream(1)));
    let Incoming::Payload(p) = daemon.classify(&f).unwrap() else {
        panic!("payload");
    };
    let Delivery::Deliver(delivered) = daemon.decode::<SearchToDaemonMsg>(&p).unwrap() else {
        panic!("delivered");
    };
    assert_eq!(delivered.stream(), stream(1));
}

// ── region and notify streams ───────────────────────────────────────

#[test]
fn every_streaming_family_shares_one_table() {
    let mut driver = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Ops);
    let rows = correlated(
        &RegionToDaemonMsg::Rows {
            source: RegionSource::Scrollback,
            ansi: false,
            max_rows: None,
        },
        Correlation::stream(stream(1)),
    );
    let Incoming::Payload(p) = driver.classify(&rows).unwrap() else {
        panic!("payload");
    };
    assert!(matches!(
        driver.decode::<RegionToDaemonMsg>(&p).unwrap(),
        Delivery::Deliver(_)
    ));

    let mut observer = in_phase::<DaemonSide>(Phase::Setup, ConnectionMode::Observer);
    let subscribe = correlated(
        &NotifyToDaemonMsg::Subscribe {
            session_prefix: None,
        },
        Correlation::stream(stream(1)),
    );
    let Incoming::Payload(p) = observer.classify(&subscribe).unwrap() else {
        panic!("payload");
    };
    assert!(matches!(
        observer.decode::<NotifyToDaemonMsg>(&p).unwrap(),
        Delivery::Deliver(_)
    ));
    assert_eq!(observer.outstanding_streams(), 1);
}

// ── the phase ladder ────────────────────────────────────────────────

/// One frame per family, body-valid and envelope-free, so a cell of the
/// admission table reports the kind-level gate rather than a family's
/// own correlation rules. `Conn` is absent on purpose: its arms are
/// decoded inside `classify`, so its cells are arm-level and are pinned
/// by [`the_conn_arms_are_each_legal_in_their_own_phases`].
fn sample_of(kind: MessageKind) -> OwnedFrame {
    use felis_protocol::{
        ImageId,
        messages::{GridMsg, ImageMsg, InputMsg, PushMsg},
    };
    match kind {
        MessageKind::Input => frame(&InputMsg::KeyBytes(b"a".to_vec())),
        MessageKind::Grid => frame(&GridMsg::RehydrateBegin),
        MessageKind::Image => frame(&ImageMsg::Delete { id: ImageId(7) }),
        MessageKind::Session => frame(&SessionToDaemonMsg::Detach),
        MessageKind::Ops => frame(&OpsToDaemonMsg::List),
        MessageKind::Region => frame(&RegionToDaemonMsg::Request {
            source: RegionSource::Visible,
            ansi: false,
        }),
        MessageKind::Notify => frame(&NotifyToDaemonMsg::Subscribe {
            session_prefix: None,
        }),
        MessageKind::Push => frame(&PushMsg::Evicted {
            reason: "evicted".into(),
        }),
        MessageKind::Search => frame(&a_query()),
        MessageKind::Conn => unreachable!("Conn is covered arm by arm"),
    }
}

const EVERY_KIND: [MessageKind; 9] = [
    MessageKind::Input,
    MessageKind::Grid,
    MessageKind::Image,
    MessageKind::Session,
    MessageKind::Ops,
    MessageKind::Region,
    MessageKind::Notify,
    MessageKind::Push,
    MessageKind::Search,
];

const EVERY_MODE: [ConnectionMode; 3] = [
    ConnectionMode::Window,
    ConnectionMode::Ops,
    ConnectionMode::Observer,
];

/// The matrix the docs publish, written out by hand, against the folds
/// the driver actually reads, so an arm whose `ModeSet`/`PhaseSet`
/// silently widens its family's surface fails here rather than being
/// asserted against itself. Per-arm rows are pinned one level down, by
/// `the_schema_declares_the_same_arm_table` against `felis.proto`.
#[test]
fn the_kind_folds_are_the_matrix_the_reference_publishes() {
    // (kind, modes, phases), from `docs/reference/ipc.md` "Connection
    // phases" and "Connection modes".
    let published: [(MessageKind, &[&str], &[&str]); 10] = [
        (
            MessageKind::Conn,
            &["window", "ops", "observer"],
            &["handshake", "setup", "attached", "observing"],
        ),
        (MessageKind::Input, &["window", "ops"], &["attached"]),
        (MessageKind::Grid, &["window", "ops"], &["attached"]),
        (MessageKind::Image, &["window", "ops"], &["attached"]),
        (
            MessageKind::Session,
            &["window", "ops"],
            &["setup", "attached"],
        ),
        (MessageKind::Ops, &["window", "ops"], &["setup", "attached"]),
        (MessageKind::Region, &["window", "ops"], &["attached"]),
        (MessageKind::Notify, &["observer"], &["setup", "observing"]),
        (MessageKind::Push, &["window", "ops"], &["attached"]),
        (MessageKind::Search, &["window", "ops"], &["attached"]),
    ];
    for (kind, modes, phases) in published {
        assert_eq!(kind.modes().tokens(), modes, "{kind} modes");
        assert_eq!(kind.phases().tokens(), phases, "{kind} phases");
    }
}

/// Test every `(side, mode, phase, kind)` cell of the pre-decode gate.
///
/// Verifies that all columns are consulted and errors are ordered: mode,
/// phase, then direction.
#[test]
fn the_kind_gate_is_the_arm_tables_fold_in_every_cell() {
    fn every_cell<R: Role>() {
        let side = R::SIDE;
        for mode in EVERY_MODE {
            for phase in [Phase::Setup, Phase::Attached, Phase::Observing] {
                for kind in EVERY_KIND {
                    let mut driver = in_phase::<R>(phase, mode);
                    let outcome = driver.classify(&sample_of(kind));
                    let mode_ok = kind.modes().contains(mode);
                    let phase_ok = kind.phases().contains(phase);
                    let direction_ok = kind
                        .sole_direction()
                        .is_none_or(|one_way| one_way == side.inbound());
                    let cell = format!("{side:?}/{mode:?}/{phase:?}/{kind}");
                    match (mode_ok, phase_ok, direction_ok) {
                        (false, _, _) => assert!(
                            matches!(outcome, Err(DriverError::ModeDenied { .. })),
                            "{cell}: a surface this mode never asked for is a mode denial"
                        ),
                        (true, false, _) => assert!(
                            matches!(outcome, Err(DriverError::OutOfPhase { .. })),
                            "{cell}: legal in {:?} only",
                            kind.phases().tokens()
                        ),
                        (true, true, false) => assert!(
                            matches!(outcome, Err(DriverError::WrongDirection { .. })),
                            "{cell}: a one-way family arriving backwards"
                        ),
                        (true, true, true) => assert!(
                            matches!(outcome, Ok(Incoming::Payload(_))),
                            "{cell}: every column admits it, got {outcome:?}"
                        ),
                    }
                }
            }
        }
    }
    every_cell::<ClientSide>();
    every_cell::<DaemonSide>();
}

/// The handshake admits its own two arms and nothing else, and the
/// preface admits no frame at all: the two rungs below `Setup`.
#[test]
fn the_two_rungs_below_setup_admit_almost_nothing() {
    for kind in EVERY_KIND {
        let mut fresh = ConnectionDriver::daemon();
        assert!(
            matches!(
                fresh.classify(&sample_of(kind)),
                Err(DriverError::UnexpectedKind { .. })
            ),
            "{kind} before the preface"
        );
        let mut handshaking = ConnectionDriver::daemon();
        handshaking.preface_done();
        assert!(
            matches!(
                handshaking.classify(&sample_of(kind)),
                Err(DriverError::OutOfPhase { .. })
            ),
            "{kind} during the handshake"
        );
    }
}

/// `Conn` is legal in every phase as a kind, so its rows are arm-level:
/// the handshake pair only in the handshake, the stream control arms
/// only where a stream can be live, and the refusal wherever the daemon
/// may need to write one.
#[test]
fn the_conn_arms_are_each_legal_in_their_own_phases() {
    // A live stream, so the terminal's own accounting cannot be what
    // refuses it, and the phase column is what the case reports.
    let end = |phase| {
        let mut client = in_phase::<ClientSide>(phase, ConnectionMode::Ops);
        let id = client.open_stream().expect("the sequence is fresh");
        client.classify(&frame(&ConnToClientMsg::End {
            stream_id: id,
            count: 0,
        }))
    };
    for phase in [Phase::Setup, Phase::Attached, Phase::Observing] {
        assert!(
            matches!(
                end(phase),
                Ok(Incoming::Control(ConnToClientMsg::End { .. }))
            ),
            "a stream terminal belongs to every phase a stream can live in ({phase:?})"
        );
    }

    let mut welcomed = in_phase::<ClientSide>(Phase::Setup, ConnectionMode::Ops);
    let err = welcomed
        .classify(&frame(&ConnToClientMsg::Welcome { identity: None }))
        .unwrap_err();
    assert!(
        matches!(&err, DriverError::OutOfPhase { arm, legal, .. }
            if *arm == "Conn::Welcome" && legal == "handshake"),
        "got {err}"
    );

    // The one arm with no phase of its own: a refusal may answer a
    // `Hello`, a pre-attach verb, or an attached connection's frame.
    for phase in [Phase::Setup, Phase::Attached, Phase::Observing] {
        let mut client = in_phase::<ClientSide>(phase, ConnectionMode::Ops);
        assert!(
            matches!(
                client.classify(&frame(&ConnToClientMsg::Refused {
                    reason: felis_protocol::messages::RefusalReason::Role,
                    detail: String::new(),
                })),
                Ok(Incoming::Control(ConnToClientMsg::Refused { .. }))
            ),
            "a refusal is legal in {phase:?}"
        );
    }
}

/// The openers belong to `Setup`, what attaching earns belongs to
/// `Attached`, and neither leaks into the other.
#[test]
fn the_session_openers_and_the_attached_traffic_do_not_overlap() {
    let opener = SessionToDaemonMsg::Attach {
        target: felis_protocol::messages::AttachTarget::Id(1),
        live_only: false,
    };
    let mut setup = in_phase::<DaemonSide>(Phase::Setup, ConnectionMode::Window);
    let Incoming::Payload(p) = setup.classify(&frame(&opener)).unwrap() else {
        panic!("a session opener is a payload");
    };
    assert!(matches!(
        setup.decode::<SessionToDaemonMsg>(&p).unwrap(),
        Delivery::Deliver(_)
    ));

    // A second attach: refused by the table, not by a `match` arm in
    // the daemon's pump.
    let mut attached = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Window);
    let Incoming::Payload(p) = attached.classify(&frame(&opener)).unwrap() else {
        panic!("a session opener is a payload");
    };
    let err = attached.decode::<SessionToDaemonMsg>(&p).unwrap_err();
    assert!(
        matches!(&err, DriverError::OutOfPhase { arm, legal, .. }
            if *arm == "Session::Attach" && legal == "setup"),
        "got {err}"
    );

    let mut setup = in_phase::<DaemonSide>(Phase::Setup, ConnectionMode::Window);
    let Incoming::Payload(p) = setup.classify(&frame(&SessionToDaemonMsg::Detach)).unwrap() else {
        panic!("a detach is a payload");
    };
    let err = setup.decode::<SessionToDaemonMsg>(&p).unwrap_err();
    assert!(
        matches!(&err, DriverError::OutOfPhase { arm, legal, .. }
            if *arm == "Session::Detach" && legal == "attached"),
        "got {err}"
    );
}

/// An observer and an attached window take part in disjoint
/// conversations: neither the grid burst nor a notification crosses.
#[test]
fn an_observer_and_an_attached_window_share_no_surface() {
    use felis_protocol::messages::GridMsg;

    // A grid push before the attach.
    let mut setup = in_phase::<ClientSide>(Phase::Setup, ConnectionMode::Window);
    let err = setup
        .classify(&frame(&GridMsg::RehydrateBegin))
        .unwrap_err();
    assert!(
        matches!(&err, DriverError::OutOfPhase { arm, legal, .. }
            if *arm == "Grid" && legal == "attached"),
        "got {err}"
    );

    // The subscribe is the transition, so it is legal once and not again.
    let mut observer = in_phase::<DaemonSide>(Phase::Setup, ConnectionMode::Observer);
    let id = stream(1);
    let subscribe = correlated(
        &NotifyToDaemonMsg::Subscribe {
            session_prefix: None,
        },
        Correlation::stream(id),
    );
    let Incoming::Payload(p) = observer.classify(&subscribe).unwrap() else {
        panic!("a subscribe is a payload");
    };
    assert!(matches!(
        observer.decode::<NotifyToDaemonMsg>(&p).unwrap(),
        Delivery::Deliver(_)
    ));
    observer.observing();
    let second = correlated(
        &NotifyToDaemonMsg::Subscribe {
            session_prefix: None,
        },
        Correlation::stream(stream(2)),
    );
    let Incoming::Payload(p) = observer.classify(&second).unwrap() else {
        panic!("a subscribe is a payload");
    };
    assert!(
        matches!(
            observer.decode::<NotifyToDaemonMsg>(&p),
            Err(DriverError::OutOfPhase { .. })
        ),
        "a second subscribe is out of phase once the role exists"
    );
}

/// The `Window` mutation is denied by mode in both phases. Which shape
/// the refusal takes (a `Conn::Refused` that ends the connection, or a
/// typed error riding the request envelope) is the caller's, and the
/// driver reports the same typed denial to both.
#[test]
fn a_denied_ops_verb_reads_the_same_before_and_after_the_attach() {
    for phase in [Phase::Setup, Phase::Attached] {
        let mut daemon = in_phase::<DaemonSide>(phase, ConnectionMode::Window);
        let verb = OpsToDaemonMsg::Destroy {
            id_prefix: "ab".into(),
        };
        let f = correlated(&verb, Correlation::request(request(1)));
        let Incoming::Payload(p) = daemon.classify(&f).unwrap() else {
            panic!("an Ops verb is a payload");
        };
        let err = daemon.decode::<OpsToDaemonMsg>(&p).unwrap_err();
        assert!(
            matches!(&err, DriverError::ArmDenied { mode: ConnectionMode::Window, arm, .. }
                if *arm == "Ops::Destroy"),
            "{phase:?}: got {err}"
        );
    }
}

/// A parked frame must reach the driver exactly once: the client's
/// correlated wait re-feeds what it read past, and a frame counted twice
/// settles a stream transition twice. The second feed of one frame is
/// therefore a correlation violation, never a second delivery.
#[test]
fn feeding_one_frame_twice_is_a_correlation_violation() {
    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let id = client.open_stream().expect("the sequence is fresh");
    let terminal = frame(&ConnToClientMsg::End {
        stream_id: id,
        count: 1,
    });
    assert!(matches!(
        client.classify(&terminal).unwrap(),
        Incoming::Control(ConnToClientMsg::End { .. })
    ));
    assert!(matches!(
        client.classify(&terminal).unwrap_err(),
        DriverError::Correlation { .. }
    ));
}

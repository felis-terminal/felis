use super::*;

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
            source: felis_protocol::messages::SearchSource::Scrollback,
            session: None,
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
                session_prefix: None,
                kinds: Vec::new(),
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
        PushMsg::SessionExited {
            id: 1,
            status: None,
        },
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

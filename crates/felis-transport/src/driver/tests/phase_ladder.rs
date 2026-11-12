use super::*;

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
            session: None,
        }),
        MessageKind::Notify => frame(&NotifyToDaemonMsg::Subscribe {
            session_prefix: None,
            kinds: Vec::new(),
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
/// asserted against itself. Per-arm rows are generated from
/// `felis.proto`'s arm options.
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
        (
            MessageKind::Region,
            &["window", "ops"],
            &["setup", "attached"],
        ),
        (MessageKind::Notify, &["observer"], &["setup", "observing"]),
        (MessageKind::Push, &["window", "ops"], &["attached"]),
        (
            MessageKind::Search,
            &["window", "ops"],
            &["setup", "attached"],
        ),
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

/// The handshake admits its own two arms and nothing else.
#[test]
fn the_handshake_rung_admits_almost_nothing() {
    for kind in EVERY_KIND {
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
        image_regions: false,
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
            kinds: Vec::new(),
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
            kinds: Vec::new(),
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

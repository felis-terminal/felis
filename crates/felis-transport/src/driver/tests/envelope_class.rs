use super::*;

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
            frame_correlated(&a_reply, Correlation::stream(stream(1))),
            "a reply is not a stream item",
        ),
        (
            frame_correlated(&a_row, request),
            "a stream item is not a reply",
        ),
    ] {
        assert!(
            matches!(
                decode_frame::<RegionToClientMsg>(&mut client, &frame),
                Err(DriverError::Correlation { .. })
            ),
            "{why}"
        );
    }

    // `Session` carries the envelope slot and never fills it, and its
    // uncorrelated arms are held to that.
    let mut daemon = in_phase::<DaemonSide>(Phase::Attached, ConnectionMode::Window);
    assert!(matches!(
        decode_frame::<SessionToDaemonMsg>(
            &mut daemon,
            &frame_correlated(&SessionToDaemonMsg::Detach, request)
        ),
        Err(DriverError::Correlation { .. })
    ));

    assert!(
        matches!(
            decode_frame::<OpsToDaemonMsg>(&mut daemon, &frame(&OpsToDaemonMsg::List)),
            Err(DriverError::Correlation { .. })
        ),
        "a request with no request_id can never be answered"
    );
}

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
        session: None,
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
            frame_correlated(&SessionToDaemonMsg::Detach, request_envelope()),
        ),
        (
            "Ops::List opens a request",
            Side::Daemon,
            ConnectionMode::Ops,
            Phase::Attached,
            frame_correlated(&OpsToDaemonMsg::List, stream_envelope()),
        ),
        (
            "Ops::Listed answers one",
            Side::Client,
            ConnectionMode::Ops,
            Phase::Attached,
            frame_correlated(
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
            frame_correlated(
                &RegionToDaemonMsg::Request {
                    source: RegionSource::Scrollback,
                    ansi: false,
                    session: None,
                },
                stream_envelope(),
            ),
        ),
        (
            "Region::Reply answers one",
            Side::Client,
            ConnectionMode::Window,
            Phase::Attached,
            frame_correlated(&a_region_reply(), stream_envelope()),
        ),
        (
            "Region::Rows opens a stream",
            Side::Daemon,
            ConnectionMode::Window,
            Phase::Attached,
            frame_correlated(&a_region_rows(), request_envelope()),
        ),
        (
            "Region::Row is a stream item",
            Side::Client,
            ConnectionMode::Window,
            Phase::Attached,
            frame_correlated(&a_region_row(), request_envelope()),
        ),
        (
            "Region::RowsDone is a stream item",
            Side::Client,
            ConnectionMode::Window,
            Phase::Attached,
            frame_correlated(
                &RegionToClientMsg::RowsDone { exit_code: None },
                request_envelope(),
            ),
        ),
        (
            "Search::Query opens a stream",
            Side::Daemon,
            ConnectionMode::Ops,
            Phase::Attached,
            frame_correlated(&a_query(), request_envelope()),
        ),
        (
            "Search::Match is a stream item",
            Side::Client,
            ConnectionMode::Ops,
            Phase::Attached,
            frame_correlated(&a_match(), request_envelope()),
        ),
        (
            "Notify::Subscribe opens a stream",
            Side::Daemon,
            ConnectionMode::Observer,
            Phase::Setup,
            frame_correlated(
                &NotifyToDaemonMsg::Subscribe {
                    session_prefix: None,
                    kinds: Vec::new(),
                },
                request_envelope(),
            ),
        ),
        (
            "Notify::Lagged is a stream item",
            Side::Client,
            ConnectionMode::Observer,
            Phase::Observing,
            frame_correlated(&NotifyToClientMsg::Lagged { missed: 1 }, request_envelope()),
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
    let f = frame_correlated(&OpsToDaemonMsg::List, Correlation::stream(stream(1)));
    let err = decode_frame::<OpsToDaemonMsg>(&mut daemon, &f).unwrap_err();
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
    let f = frame_correlated(
        &SessionToDaemonMsg::Detach,
        Correlation::request(request(1)),
    );
    let err = decode_frame::<SessionToDaemonMsg>(&mut daemon, &f).unwrap_err();
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
    let f = frame_correlated(&OpsToDaemonMsg::List, envelope);
    let receiver = decode_frame::<OpsToDaemonMsg>(&mut daemon, &f).unwrap_err();

    assert_eq!(sender.to_string(), receiver.to_string());
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
    let f = with_hand_written_ids(&InputMsg::FocusChange { focused: true }, REQUEST_ID_1);
    let err = decode_frame::<InputMsg>(&mut daemon, &f).unwrap_err();
    assert!(
        matches!(&err, DriverError::Correlation { expected, .. }
            if expected.contains("no correlation envelope")),
        "got {err}"
    );

    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let f = with_hand_written_ids(&GridMsg::RehydrateBegin, REQUEST_ID_1);
    assert!(matches!(
        decode_frame::<GridMsg>(&mut client, &f).unwrap_err(),
        DriverError::Correlation { .. }
    ));

    let f = with_hand_written_ids(&ImageMsg::Delete { id: ImageId(7) }, REQUEST_ID_1);
    assert!(matches!(
        decode_frame::<ImageMsg>(&mut client, &f).unwrap_err(),
        DriverError::Correlation { .. }
    ));

    let f = with_hand_written_ids(
        &PushMsg::Evicted {
            reason: "evicted".into(),
        },
        REQUEST_ID_1,
    );
    assert!(matches!(
        decode_frame::<PushMsg>(&mut client, &f).unwrap_err(),
        DriverError::Correlation { .. }
    ));

    // `Conn` never becomes a payload, so its check runs in `classify`.
    let live = client.open_stream().expect("the sequence is fresh");
    let err = client
        .classify(&with_hand_written_ids(
            &ConnToClientMsg::End {
                stream_id: live,
                count: 0,
            },
            REQUEST_ID_1,
        ))
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

/// A malformed envelope is malformed on every family (REQ-113,
/// REQ-114), including the uncorrelated ones. The fault is raised only
/// once the family decode has succeeded, which is what proves the
/// envelope alone was what failed.
#[test]
fn a_malformed_envelope_closes_the_connection_on_an_uncorrelated_family() {
    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let f = with_field_100(&GridMsg::RehydrateBegin, MALFORMED_ENVELOPE);
    let p = payload_of(&mut client, &f);
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
        .classify(&with_field_100(
            &ConnToClientMsg::End {
                stream_id: live,
                count: 0,
            },
            MALFORMED_ENVELOPE,
        ))
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
    assert!(matches!(
        decode_frame::<GridMsg>(&mut client, &f).unwrap_err(),
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
    assert!(matches!(
        decode_frame::<OpsToDaemonMsg>(&mut daemon, &f).unwrap_err(),
        DriverError::Correlation { .. }
    ));

    let f = frame(&a_query());
    assert!(matches!(
        decode_frame::<SearchToDaemonMsg>(&mut daemon, &f).unwrap_err(),
        DriverError::Correlation { .. }
    ));

    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let f = frame(&a_match());
    assert!(matches!(
        decode_frame::<SearchToClientMsg>(&mut client, &f).unwrap_err(),
        DriverError::Correlation { .. }
    ));
}

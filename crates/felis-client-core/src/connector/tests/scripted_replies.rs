use super::*;

/// A connection whose peer is a script, so a reply the daemon would
/// never write can be put on the wire.
fn scripted_peer(
    frames: Vec<(MessageKind, Vec<u8>)>,
) -> Connection<
    tokio::io::ReadHalf<tokio::io::DuplexStream>,
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
> {
    let (client_side, peer_side) = tokio::io::duplex(64 * 1024);
    let (peer_read, peer_write) = tokio::io::split(peer_side);
    tokio::spawn(async move {
        // The verb's own frame is read and dropped: the script
        // answers by position, not by what it was asked.
        let mut reader = FrameReader::new(peer_read);
        let mut writer = FrameWriter::at_build_minor(peer_write);
        for (kind, body) in frames {
            let _request = reader.next_frame().await;
            let _written = writer
                .write_frame_unchecked(&Frame {
                    kind: kind.as_u16(),
                    body: &body,
                })
                .await;
            let _flushed = writer.flush().await;
        }
    });
    let (client_read, client_write) = tokio::io::split(client_side);
    Connection::from_halves(
        FrameReader::new(client_read),
        FrameWriter::at_build_minor(client_write),
        ConnectionMode::Ops,
    )
}

/// A reply naming an id this side never awaited is a duplicate,
/// late or unattributable answer. Parked as "not my reply" it would
/// be requeued in front of every later verb and never decoded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reply_naming_another_request_ends_the_connection() {
    let stray = codec::encode_correlated(
        &OpsToClientMsg::Listed {
            sessions: Vec::new(),
        },
        Correlation::request(RequestId::new(7).unwrap()),
    );
    let mut conn = scripted_peer(vec![(MessageKind::Ops, stray)]);
    assert!(
        matches!(
            conn.list_sessions().await,
            Err(ConnectError::Driver(DriverError::Correlation { .. }))
        ),
        "a reply for request 7 cannot answer request 1",
    );
}

/// A reply arm with its envelope omitted. Parked as "not my reply"
/// the verb would wait out its deadline, or forever without one,
/// for an answer already on the wire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reply_without_an_envelope_ends_the_connection() {
    let envelopeless = codec::encode(&OpsToClientMsg::Listed {
        sessions: Vec::new(),
    });
    let mut conn = scripted_peer(vec![(MessageKind::Ops, envelopeless)]);
    assert!(
        matches!(
            conn.list_sessions().await,
            Err(ConnectError::Driver(DriverError::Correlation { .. }))
        ),
        "an answer that names no request answers nothing",
    );
}

/// The request-scoped refusal is an answer too, so a stray one is
/// as unattributable as a stray reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_scoped_error_naming_another_request_ends_the_connection() {
    let stray = codec::encode(&ConnToClientMsg::Error {
        subject: Subject::Request(RequestId::new(7).unwrap()),
        reason: StreamErrorReason::Internal,
        detail: "not this request".to_owned(),
    });
    let mut conn = scripted_peer(vec![(MessageKind::Conn, stray)]);
    assert!(
        matches!(
            conn.list_sessions().await,
            Err(ConnectError::Driver(DriverError::Correlation { .. }))
        ),
        "a refusal of request 7 cannot answer request 1",
    );
}

/// Every `Conn` arm is uncorrelated: the refusal names its subject
/// inline, so a field-100 envelope on it is correlation no arm
/// claims, and interpreting it before the driver has admitted the
/// frame would leave the connection running on one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_carrying_an_envelope_ends_the_connection() {
    let mut body = codec::encode(&ConnToClientMsg::Error {
        subject: Subject::Request(RequestId::new(1).unwrap()),
        reason: StreamErrorReason::Internal,
        detail: "refused".to_owned(),
    });
    // Tag 100, wire type 2, wrapping a `Correlation` naming request 1.
    body.extend_from_slice(&[0xA2, 0x06, 0x02, 0x08, 0x01]);
    let mut conn = scripted_peer(vec![(MessageKind::Conn, body)]);
    assert!(
        matches!(
            conn.list_sessions().await,
            Err(ConnectError::Driver(DriverError::Correlation { .. }))
        ),
        "a control arm carrying an envelope is unattributable correlation",
    );
}

#[test]
fn force_detach_reply_lands_the_resolution_then_the_eviction_flag() {
    let reply = OpsToClientMsg::Detached {
        resolved: ResolvedId::Ok { id: 0xABCD },
        was_attached: true,
    };
    let (resolved, was_attached) = ForceDetachReq {
        id_prefix: "abcd".into(),
    }
    .interpret(reply)
    .unwrap();
    assert_eq!(resolved, ResolvedId::Ok { id: 0xABCD });
    assert!(was_attached);

    let reply = OpsToClientMsg::Detached {
        resolved: ResolvedId::Ambiguous { matches: 3 },
        was_attached: false,
    };
    let (resolved, was_attached) = ForceDetachReq {
        id_prefix: "abcd".into(),
    }
    .interpret(reply)
    .unwrap();
    assert_eq!(resolved, ResolvedId::Ambiguous { matches: 3 });
    assert!(!was_attached);
}

fn switch_req() -> SwitchSessionReq {
    SwitchSessionReq {
        from_prefix: "1111".into(),
        target: SwitchTarget::Session("2222".to_owned()),
        scope: SwitchScope::Default,
    }
}

#[test]
fn switch_reply_lands_from_then_to_then_the_queued_count() {
    let reply = OpsToClientMsg::Switched {
        from: ResolvedId::Ok { id: 0x1111 },
        to: Some(ResolvedId::Ok { id: 0x2222 }),
        queued: 7,
        denied: None,
    };
    let got = switch_req().interpret(reply).unwrap();
    assert_eq!(got.from, ResolvedId::Ok { id: 0x1111 });
    assert_eq!(got.to, Some(ResolvedId::Ok { id: 0x2222 }));
    assert_eq!(got.queued, 7);
    assert_eq!(got.denied, None);

    let reply = OpsToClientMsg::Switched {
        from: ResolvedId::Ok { id: 0x3333 },
        to: None,
        queued: 2,
        denied: None,
    };
    let got = switch_req().interpret(reply).unwrap();
    assert_eq!(got.from, ResolvedId::Ok { id: 0x3333 });
    assert_eq!(got.to, None);
    assert_eq!(got.queued, 2);
}

#[test]
fn switch_reply_carries_a_denial_beside_the_zero_count() {
    let reply = OpsToClientMsg::Switched {
        from: ResolvedId::Ok { id: 0x4444 },
        to: Some(ResolvedId::Ok { id: 0x5555 }),
        queued: 0,
        denied: Some(SwitchDenied::NoSuchAttachment { attachment: 12 }),
    };
    let got = switch_req().interpret(reply).unwrap();
    assert_eq!(got.queued, 0);
    assert_eq!(
        got.denied,
        Some(SwitchDenied::NoSuchAttachment { attachment: 12 })
    );
}

/// Every (context, arm, value) triple, against what `serve.rs` can
/// actually produce for each request form.
#[test]
fn a_request_admits_only_the_refusals_it_can_earn() {
    const ATTACH: [AttachFailure; 5] = [
        AttachFailure::UnknownSession,
        AttachFailure::SessionEnding,
        AttachFailure::SessionExited,
        AttachFailure::NoMatch,
        AttachFailure::Ambiguous,
    ];
    const CREATE: [CreateFailure; 4] = [
        CreateFailure::SpawnFailed,
        CreateFailure::GeometryOutOfRange,
        CreateFailure::SessionLimitReached,
        CreateFailure::DaemonDraining,
    ];

    let by_id = |live_only| SessionRefusalContext::Attach {
        by_prefix: false,
        live_only,
    };
    let by_prefix = |live_only| SessionRefusalContext::Attach {
        by_prefix: true,
        live_only,
    };
    let table = [
        (
            by_id(false),
            vec![
                AttachRefusal::Attach(AttachFailure::UnknownSession),
                AttachRefusal::Attach(AttachFailure::SessionEnding),
            ],
        ),
        (
            by_id(true),
            vec![
                AttachRefusal::Attach(AttachFailure::UnknownSession),
                AttachRefusal::Attach(AttachFailure::SessionEnding),
                AttachRefusal::Attach(AttachFailure::SessionExited),
            ],
        ),
        (
            by_prefix(false),
            vec![
                AttachRefusal::Attach(AttachFailure::NoMatch),
                AttachRefusal::Attach(AttachFailure::Ambiguous),
                AttachRefusal::Attach(AttachFailure::SessionEnding),
            ],
        ),
        (
            by_prefix(true),
            vec![
                AttachRefusal::Attach(AttachFailure::NoMatch),
                AttachRefusal::Attach(AttachFailure::Ambiguous),
                AttachRefusal::Attach(AttachFailure::SessionEnding),
                AttachRefusal::Attach(AttachFailure::SessionExited),
            ],
        ),
        (
            SessionRefusalContext::Create,
            std::iter::once(AttachRefusal::Attach(AttachFailure::SessionEnding))
                .chain(CREATE.map(AttachRefusal::Create))
                .collect(),
        ),
    ];

    for (ctx, admitted) in table {
        for value in ATTACH
            .map(AttachRefusal::Attach)
            .into_iter()
            .chain(CREATE.map(AttachRefusal::Create))
        {
            assert_eq!(
                admits(ctx, value),
                admitted.contains(&value),
                "{ctx:?} against {value:?}"
            );
        }
    }
}

/// A refusal the request could not have earned retires the
/// connection: the next verb is answered from this side's own
/// state, although a valid reply for it is already on the wire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_verb_after_an_inadmissible_refusal_never_reaches_the_socket() {
    let (client_side, peer_side) = tokio::io::duplex(64 * 1024);
    let (peer_read, peer_write) = tokio::io::split(peer_side);
    tokio::spawn(async move {
        let mut reader = FrameReader::new(peer_read);
        let mut writer = FrameWriter::at_build_minor(peer_write);
        let _attach = reader.next_frame().await;
        let _refusal = writer
            .send(&SessionToClientMsg::AttachFailed {
                reason: AttachRefusal::Create(CreateFailure::DaemonDraining),
                detail: String::new(),
            })
            .await;
        let _listed = writer
            .send_correlated(
                &OpsToClientMsg::Listed {
                    sessions: Vec::new(),
                },
                Correlation::request(RequestId::new(1).unwrap()),
            )
            .await;
        let _flushed = writer.flush().await;
        // Held open so a read would block rather than report EOF.
        std::future::pending::<()>().await;
    });
    let (client_read, client_write) = tokio::io::split(client_side);
    let mut conn = Connection::from_halves(
        FrameReader::new(client_read),
        FrameWriter::at_build_minor(client_write),
        ConnectionMode::Ops,
    );

    let err = conn
        .attach(0xCAFE, AttachIntent::Deliberate)
        .await
        .expect_err("a spawn-half refusal cannot answer an attach");
    assert!(
        matches!(err, ConnectError::InadmissibleRefusal { .. }),
        "{err:?}"
    );
    assert!(
        matches!(conn.list_sessions().await, Err(ConnectError::Abandoned)),
        "the waiting `Listed` must not be read off a connection this side abandoned"
    );
}

#[test]
fn tag_reply_lands_the_resolution_then_the_resulting_tag_set() {
    let reply = OpsToClientMsg::TagsUpdated {
        resolved: ResolvedId::Ok { id: 0x5A },
        tags: vec!["agent".to_owned(), "work".to_owned()],
        denied: None,
    };
    let (resolved, tags, denied) = TagReq {
        id_prefix: "5a".into(),
        add: Vec::new(),
        remove: Vec::new(),
    }
    .interpret(reply)
    .unwrap();
    assert_eq!(resolved, ResolvedId::Ok { id: 0x5A });
    assert_eq!(tags, ["agent", "work"]);
    assert_eq!(denied, None);
}

#[test]
fn an_ops_reply_of_the_wrong_variant_is_refused_by_each_request() {
    let stray = || OpsToClientMsg::Listed {
        sessions: Vec::new(),
    };
    assert!(matches!(
        ForceDetachReq {
            id_prefix: "aa".into()
        }
        .interpret(stray()),
        Err(ConnectError::NotSessionAttached),
    ));
    assert!(matches!(
        switch_req().interpret(stray()),
        Err(ConnectError::NotSessionAttached),
    ));
    assert!(matches!(
        TagReq {
            id_prefix: "aa".into(),
            add: Vec::new(),
            remove: Vec::new(),
        }
        .interpret(stray()),
        Err(ConnectError::NotSessionAttached),
    ));
}

async fn attach_against_scripted_reply<M>(reply: M) -> ConnectError
where
    M: codec::Correlated + Send + 'static,
{
    use felis_transport::{FrameReader, FrameWriter};

    let (client_side, daemon_side) = tokio::io::duplex(64 * 1024);
    let (client_read, client_write) = tokio::io::split(client_side);
    let (daemon_read, daemon_write) = tokio::io::split(daemon_side);

    let daemon = tokio::spawn(async move {
        let mut reader = FrameReader::new(daemon_read);
        let mut writer = FrameWriter::at_build_minor(daemon_write);
        let attach = reader
            .next_frame()
            .await
            .expect("read the attach")
            .expect("the client sends one");
        assert_eq!(attach.kind, MessageKind::Session.as_u16());
        // The envelope on an uncorrelated arm is the point of the
        // first case, and the encoder refuses to write it: only a
        // malformed peer produces this frame, so it is hand-built.
        let body =
            codec::encode_correlated(&reply, Correlation::request(RequestId::new(1).unwrap()));
        writer
            .write_frame_unchecked(&Frame {
                kind: MessageKind::Session.as_u16(),
                body: &body,
            })
            .await
            .expect("write the scripted reply");
        writer.flush().await.expect("flush the scripted reply");
    });

    let mut conn = Connection::from_halves(
        FrameReader::new(client_read),
        FrameWriter::at_build_minor(client_write),
        ConnectionMode::Window,
    );
    let err = conn
        .attach(1, AttachIntent::Deliberate)
        .await
        .expect_err("the reply violates the arm table");
    daemon.await.expect("daemon task");
    err
}

/// `Session::AttachFailed` is an uncorrelated arm, so a reply
/// carrying a `request_id` is unattributable corruption (REQ-114),
/// not a refusal to report.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_correlated_session_reply_ends_the_connection() {
    let err = attach_against_scripted_reply(SessionToClientMsg::AttachFailed {
        reason: AttachRefusal::Attach(AttachFailure::UnknownSession),
        detail: String::new(),
    })
    .await;
    assert!(
        matches!(err, ConnectError::Driver(DriverError::Correlation { .. })),
        "expected a correlation violation, got {err:?}"
    );
}

/// A frame parked while a verb is outstanding is judged by the same
/// table as one the caller consumes: this connection never attached,
/// so the whole `Push` family is out of phase and the client must
/// not requeue the frame unread and report the verb's success
/// (REQ-114).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_misrouted_push_parked_behind_a_verb_ends_the_connection() {
    use felis_protocol::messages::PushMsg;
    use felis_transport::{FrameReader, FrameWriter};

    let (client_side, daemon_side) = tokio::io::duplex(64 * 1024);
    let (client_read, client_write) = tokio::io::split(client_side);
    let (daemon_read, daemon_write) = tokio::io::split(daemon_side);

    let daemon = tokio::spawn(async move {
        let mut reader = FrameReader::new(daemon_read);
        let mut writer = FrameWriter::at_build_minor(daemon_write);
        let list = reader
            .next_frame()
            .await
            .expect("read the list request")
            .expect("the client sends one");
        assert_eq!(list.kind, MessageKind::Ops.as_u16());
        // Ahead of the reply, so the client meets it while parking.
        writer
            .send(&PushMsg::Reattach { id: 1 })
            .await
            .expect("write the misrouted push");
        writer
            .send_correlated(
                &OpsToClientMsg::Listed { sessions: vec![] },
                Correlation::request(RequestId::new(1).unwrap()),
            )
            .await
            .expect("write the reply");
    });

    let mut conn = Connection::from_halves(
        FrameReader::new(client_read),
        FrameWriter::at_build_minor(client_write),
        ConnectionMode::Ops,
    );
    let err = conn
        .list_sessions()
        .await
        .expect_err("a Window-only push on an Ops connection is fatal");
    assert!(
        matches!(
            err,
            ConnectError::Driver(DriverError::OutOfPhase { arm: "Push", .. })
        ),
        "expected a phase refusal, got {err:?}"
    );
    daemon.await.expect("daemon task");
}

/// A daemon-bound control arm arriving while a correlated verb waits
/// ends the verb as a direction fault, and the connection with it: the
/// valid reply behind it is never read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_direction_control_frame_during_a_correlated_request_retires_the_connection() {
    let cancel = codec::encode(&ConnToDaemonMsg::Cancel {
        stream_id: StreamId::new(1).unwrap(),
    });
    let welcome = codec::encode(&ConnToClientMsg::Welcome { identity: None });
    let mixed = [welcome.as_slice(), cancel.as_slice()].concat();
    for body in [cancel.clone(), mixed] {
        let (client_side, peer_side) = tokio::io::duplex(64 * 1024);
        let (peer_read, peer_write) = tokio::io::split(peer_side);
        tokio::spawn(async move {
            let mut reader = FrameReader::new(peer_read);
            let mut writer = FrameWriter::at_build_minor(peer_write);
            let _list = reader.next_frame().await;
            let _wrong_way = writer
                .write_frame_unchecked(&Frame {
                    kind: MessageKind::Conn.as_u16(),
                    body: &body,
                })
                .await;
            let _listed = writer
                .send_correlated(
                    &OpsToClientMsg::Listed {
                        sessions: Vec::new(),
                    },
                    Correlation::request(RequestId::new(1).unwrap()),
                )
                .await;
            let _flushed = writer.flush().await;
            std::future::pending::<()>().await;
        });
        let (client_read, client_write) = tokio::io::split(client_side);
        let mut conn = Connection::from_halves(
            FrameReader::new(client_read),
            FrameWriter::at_build_minor(client_write),
            ConnectionMode::Ops,
        );

        let err = conn
            .list_sessions()
            .await
            .expect_err("a daemon-bound arm cannot come from the daemon");
        assert!(
            matches!(
                &err,
                ConnectError::Driver(DriverError::WrongDirection { arm, .. }) if *arm == "Conn::Cancel"
            ),
            "{err:?}"
        );
        assert!(
            matches!(conn.list_sessions().await, Err(ConnectError::Abandoned)),
            "a connection that broke the protocol is not asked again"
        );
    }
}

/// Same path, direction column: a client that accepted a
/// client→daemon arm because `interpret` tolerated it would report
/// an attach failure for a routing violation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_direction_session_arm_ends_the_connection() {
    let err = attach_against_scripted_reply(SessionToDaemonMsg::Detach).await;
    assert!(
        matches!(
            err,
            ConnectError::Driver(DriverError::WrongDirection { .. })
        ),
        "expected a direction violation, got {err:?}"
    );
}

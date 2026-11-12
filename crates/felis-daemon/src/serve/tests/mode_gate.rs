use super::*;

/// The gate runs before the verb: a frame outside the stated mode ends the
/// connection instead of half-executing.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_window_may_not_destroy_another_session() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let id = create_parked_session(pool.clone()).await;
    assert!(
        pool.lock().await.handle_cloned(SessionId(id)).is_some(),
        "test premise: the target session is parked before the gated frame",
    );

    let result = run_gated_control_frame(
        pool.clone(),
        ConnectionMode::Window,
        OpsToDaemonMsg::Destroy { id_prefix: hex(id) },
        Some(Correlation::request(RequestId::new(1).expect("non-zero"))),
    )
    .await;
    assert_mode_denied(&result, ConnectionMode::Window, "a Destroy");

    assert!(
        pool.lock().await.handle_cloned(SessionId(id)).is_some(),
        "the refused Destroy must not have removed the target session",
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_window_may_not_force_detach_or_relay_a_switch() {
    let carrier = SwitchTarget::Carrier(felis_protocol::messages::RetargetTarget {
        carrier: felis_protocol::messages::RetargetCarrier::Ssh {
            destination: "user@devbox".into(),
            ssh_args: Vec::new(),
        },
        landing: felis_protocol::messages::RetargetLanding::Create(
            felis_protocol::messages::SpawnArgs::default(),
        ),
    });
    let gated = [
        OpsToDaemonMsg::ForceDetach {
            id_prefix: "1".into(),
        },
        OpsToDaemonMsg::Switch {
            from_prefix: "1".into(),
            target: SwitchTarget::Session("2".into()),
            scope: SwitchScope::Default,
        },
        OpsToDaemonMsg::Switch {
            from_prefix: "1".into(),
            target: carrier,
            scope: SwitchScope::Attachment(1),
        },
    ];
    for msg in gated {
        let pool = Arc::new(Mutex::new(SessionPool::new()));
        let result = run_gated_control_frame(
            pool,
            ConnectionMode::Window,
            msg.clone(),
            Some(Correlation::request(RequestId::new(1).expect("non-zero"))),
        )
        .await;
        assert_mode_denied(&result, ConnectionMode::Window, &format!("a {msg:?}"));
    }
}

/// The same denial, once the window is attached, is answered through
/// the request envelope: closing would take the user's window down over
/// one over-reaching verb.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attached_window_is_answered_an_error_for_a_mutating_verb() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let id = create_parked_session(pool.clone()).await;
    let factory = shell_factory("read _x");
    let (client_to_daemon_w, daemon_read) = tokio::io::duplex(64 * 1024);
    let (daemon_write, client_from_daemon_r) = tokio::io::duplex(64 * 1024);
    let served = pool.clone();
    let server = tokio::spawn(async move {
        drop(
            handle_stream(
                daemon_read,
                daemon_write,
                DaemonCaps::default(),
                served,
                factory,
            )
            .await,
        );
    });

    let (mut reader, mut writer) = framed(client_from_daemon_r, client_to_daemon_w).await;
    hello_welcome_as(&mut reader, &mut writer, ConnectionMode::Window, false).await;
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Attach {
            target: AttachTarget::Id(id),
            live_only: false,
            image_regions: false,
        },
    )
    .await;
    assert_eq!(attached_info(&mut reader).await.id, id);
    drain_rehydrate(&mut reader).await;

    let id_prefix = hex(id);
    send_request(&mut writer, &OpsToDaemonMsg::Destroy { id_prefix }, 1).await;
    // The window's grid stream keeps running underneath, so the answer
    // arrives behind whatever frames the session emitted meanwhile.
    let denial = loop {
        let frame = reader
            .next_frame()
            .await
            .unwrap()
            .expect("an answer, not a close");
        if frame.kind == MessageKind::Conn.as_u16() {
            break codec::decode::<ConnToClientMsg>(&frame.body).unwrap();
        }
        assert_ne!(
            frame.kind,
            MessageKind::Ops.as_u16(),
            "a Window's Destroy must not be executed",
        );
    };
    match denial {
        ConnToClientMsg::Error {
            subject: Subject::Request(request),
            reason: StreamErrorReason::InvalidRequest,
            ..
        } => assert_eq!(request.get(), 1, "the answer names the verb it refuses"),
        other => panic!("expected a typed request error, got {other:?}"),
    }
    assert!(
        pool.lock().await.handle_cloned(SessionId(id)).is_some(),
        "the refused Destroy must not have removed the session",
    );

    send_request(&mut writer, &OpsToDaemonMsg::List, 2).await;
    loop {
        let frame = reader
            .next_frame()
            .await
            .unwrap()
            .expect("the roster reply");
        if frame.kind == MessageKind::Ops.as_u16() {
            match codec::decode::<OpsToClientMsg>(&frame.body).unwrap() {
                OpsToClientMsg::Listed { .. } => break,
                other => panic!("expected OpsToClientMsg::Listed, got {other:?}"),
            }
        }
    }
    drop((writer, reader, server));
}

/// An observer's only lookup (`--session`) resolves daemon-side via
/// `NotifyToDaemonMsg::Subscribe.session_prefix`, so it is wired no roster.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_capable_modes_may_query_the_roster_an_observer_may_not() {
    for mode in [
        ConnectionMode::Window,
        ConnectionMode::Ops,
        ConnectionMode::Observer,
    ] {
        let pool = Arc::new(Mutex::new(SessionPool::new()));
        let factory = shell_factory("read _x");
        let (client_to_daemon_w, daemon_read) = tokio::io::duplex(64 * 1024);
        let (daemon_write, client_from_daemon_r) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            drop(
                handle_stream(
                    daemon_read,
                    daemon_write,
                    DaemonCaps::default(),
                    pool,
                    factory,
                )
                .await,
            );
        });
        let (mut reader, mut writer) = framed(client_from_daemon_r, client_to_daemon_w).await;
        hello_welcome_as(&mut reader, &mut writer, mode, false).await;
        send_request(&mut writer, &OpsToDaemonMsg::List, 1).await;

        let reply = reader.next_frame().await.unwrap().expect("a reply frame");
        if mode == ConnectionMode::Observer {
            let decoded = codec::decode::<ConnToClientMsg>(&reply.body).unwrap();
            assert!(
                matches!(
                    decoded,
                    ConnToClientMsg::Refused {
                        reason: RefusalReason::Role,
                        ..
                    }
                ),
                "an observer's roster query must be refused, got {decoded:?}",
            );
        } else {
            let decoded = codec::decode::<OpsToClientMsg>(&reply.body).unwrap();
            assert!(
                matches!(decoded, OpsToClientMsg::Listed { .. }),
                "{mode:?} must be answered a roster query, got {decoded:?}",
            );
        }
        drop((writer, reader, server));
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_window_may_not_subscribe_to_notifications() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let result = run_gated_control_frame(
        pool,
        ConnectionMode::Window,
        NotifyToDaemonMsg::Subscribe {
            session_prefix: None,
            kinds: Vec::new(),
        },
        Some(Correlation::stream(StreamId::new(1).expect("non-zero"))),
    )
    .await;
    assert_mode_denied(&result, ConnectionMode::Window, "a Subscribe");
}

/// The attach `Attached` reports the roster row after this subscriber joined:
/// no `idle_seconds` at all, not the parked value.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_ready_reports_the_session_as_no_longer_idle() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("read _x"));

    let id = create_parked_session(pool.clone()).await;
    // A parked row already reports `Some(0)`, so the premise is the
    // detach having landed, not any length of idleness.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while pool
        .lock()
        .await
        .handle_cloned(SessionId(id))
        .is_none_or(|handle| handle.meta_snapshot().subscribers != 0)
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the session never parked after its creator left",
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Attach {
            target: AttachTarget::Id(id),
            live_only: false,
            image_regions: false,
        },
    )
    .await;

    let ack = reader.next_frame().await.unwrap().expect("ready ack");
    match codec::decode::<SessionToClientMsg>(&ack.body).unwrap() {
        SessionToClientMsg::Attached { info } => {
            assert_eq!(
                info.idle_seconds, None,
                "Ready must reflect this subscriber, not the parked meta",
            );
        }
        other => panic!("expected Ready, got {other:?}"),
    }
}

/// A no-match filter is acked as `Subscribed { filter: NoMatch }` before
/// the close, so the CLI can exit 1 instead of reading a bare EOF
/// (docs/reference/ipc.md "Notify (kind = 7)").
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unresolvable_observer_filter_gets_a_typed_nomatch_ack() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("read _x");
    let (client_to_daemon_w, daemon_read) = tokio::io::duplex(64 * 1024);
    let (daemon_write, client_from_daemon_r) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move {
        drop(
            handle_stream(
                daemon_read,
                daemon_write,
                DaemonCaps::default(),
                pool,
                factory,
            )
            .await,
        );
    });
    let (mut reader, mut writer) = framed(client_from_daemon_r, client_to_daemon_w).await;
    hello_welcome_as(&mut reader, &mut writer, ConnectionMode::Observer, false).await;
    send_stream(
        &mut writer,
        &NotifyToDaemonMsg::Subscribe {
            session_prefix: Some("dead".into()),
            kinds: Vec::new(),
        },
        1,
    )
    .await;

    let ack = reader.next_frame().await.unwrap().expect("subscribed ack");
    let decoded = codec::decode::<NotifyToClientMsg>(&ack.body).unwrap();
    assert!(
        matches!(
            decoded,
            NotifyToClientMsg::Subscribed {
                filter: Some(ResolvedId::NoMatch)
            }
        ),
        "an unresolvable filter must ack as NoMatch, got {decoded:?}",
    );
    let terminal = reader.next_frame().await.unwrap().expect("stream terminal");
    assert!(
        matches!(
            codec::decode::<ConnToClientMsg>(&terminal.body).unwrap(),
            ConnToClientMsg::Error {
                subject: Subject::Stream(_),
                reason: StreamErrorReason::InvalidRequest,
                ..
            }
        ),
        "an unresolvable filter must end the stream with a typed error",
    );
    assert!(
        reader.next_frame().await.unwrap().is_none(),
        "the daemon closes after the terminal",
    );
    drop((writer, server));
}

/// The broadcast receiver registers before the `Subscribed` ack goes out;
/// otherwise an event published in between vanishes and `--once` waits
/// forever.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_event_published_at_the_subscribed_ack_reaches_the_observer() {
    use felis_protocol::messages::{Notification, Urgency};

    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("read _x");
    let (client_to_daemon_w, daemon_read) = tokio::io::duplex(64 * 1024);
    let (daemon_write, client_from_daemon_r) = tokio::io::duplex(64 * 1024);
    let server_pool = pool.clone();
    let server = tokio::spawn(async move {
        drop(
            handle_stream(
                daemon_read,
                daemon_write,
                DaemonCaps::default(),
                server_pool,
                factory,
            )
            .await,
        );
    });
    let (mut reader, mut writer) = framed(client_from_daemon_r, client_to_daemon_w).await;
    hello_welcome_as(&mut reader, &mut writer, ConnectionMode::Observer, false).await;
    send_stream(
        &mut writer,
        &NotifyToDaemonMsg::Subscribe {
            session_prefix: None,
            kinds: Vec::new(),
        },
        1,
    )
    .await;

    let ack = reader.next_frame().await.unwrap().expect("subscribed ack");
    assert!(matches!(
        codec::decode::<NotifyToClientMsg>(&ack.body).unwrap(),
        NotifyToClientMsg::Subscribed { filter: None }
    ));

    let hub = pool.lock().await.notify_hub();
    hub.send(NotifyToClientMsg::Event {
        session_id: 0xF00D,
        notification: Notification {
            title: Some("done".into()),
            body: "build finished".into(),
            urgency: Urgency::Normal,
        },
        notify_id: None,
        session_title: None,
        cwd: None,
        attached: false,
    })
    .expect("the observer's receiver keeps the hub open");

    let frame = tokio::time::timeout(Duration::from_secs(2), reader.next_frame())
        .await
        .expect("event read timed out")
        .unwrap()
        .expect("event frame");
    let decoded = codec::decode::<NotifyToClientMsg>(&frame.body).unwrap();
    assert!(
        matches!(decoded, NotifyToClientMsg::Event { session_id, .. } if session_id == 0xF00D),
        "expected the published event, got {decoded:?}",
    );
    drop((writer, server));
}

/// The gate runs at the kind level, ahead of the body decode.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_observer_may_not_attach() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let id = create_parked_session(pool.clone()).await;
    let result = run_gated_control_frame(
        pool,
        ConnectionMode::Observer,
        SessionToDaemonMsg::Attach {
            target: AttachTarget::Id(id),
            live_only: false,
            image_regions: false,
        },
        None,
    )
    .await;
    assert_mode_denied(&result, ConnectionMode::Observer, "an Attach");
}

/// An inbound reply half ends this connection with a typed error: skipping
/// it would leave the peer mid-conversation with a daemon that silently
/// disagreed about the last frame.
#[test]
fn request_halves_route_to_the_task_and_a_reply_half_closes_the_connection() {
    let mut driver = attached_driver();
    let mut stream = 0u64;
    let mut routed = |driver: &mut DaemonDriver, kind: MessageKind, body: Vec<u8>| {
        stream += 1;
        route_frame(
            driver,
            &OwnedFrame {
                kind: kind.as_u16(),
                body: body.into(),
            },
            SubscriberId::for_test(0),
        )
    };

    let query = codec::encode_correlated(
        &SearchToDaemonMsg::Query {
            query: "needle".to_owned(),
            options: felis_protocol::messages::SearchOptions::default(),
            source: felis_protocol::messages::SearchSource::Scrollback,
            session: None,
        },
        Correlation::stream(StreamId::new(1).unwrap()),
    );
    assert!(
        matches!(
            routed(&mut driver, MessageKind::Search, query),
            Ok(Route::Cmd(SessionCmd::Search { .. }))
        ),
        "a Search query must reach the session task",
    );

    let region = codec::encode_correlated(
        &RegionToDaemonMsg::Request {
            source: felis_protocol::messages::RegionSource::Scrollback,
            ansi: false,
            session: None,
        },
        Correlation::request(RequestId::new(1).unwrap()),
    );
    assert!(
        matches!(
            routed(&mut driver, MessageKind::Region, region),
            Ok(Route::Cmd(SessionCmd::Region { .. }))
        ),
        "a Region request must reach the session task",
    );

    let region_rows = codec::encode_correlated(
        &RegionToDaemonMsg::Rows {
            source: felis_protocol::messages::RegionSource::Scrollback,
            ansi: false,
            max_rows: None,
            session: None,
        },
        Correlation::stream(StreamId::new(2).unwrap()),
    );
    assert!(
        matches!(
            routed(&mut driver, MessageKind::Region, region_rows),
            Ok(Route::Cmd(SessionCmd::RegionRows { .. }))
        ),
        "a Region row-stream request must reach the session task",
    );

    let hit = codec::encode_correlated(
        &SearchToClientMsg::Match {
            line_index: 0,
            text: String::new(),
            byte_spans: Vec::new(),
            col_spans: Vec::new(),
            anchor: None,
        },
        Correlation::stream(StreamId::new(1).unwrap()),
    );
    let refused = routed(&mut driver, MessageKind::Search, hit);
    assert!(
        matches!(
            refused,
            Err(ConnError::Driver(DriverError::WrongDirection { .. }))
        ),
        "a reply-half Search frame must end the connection, got {refused:?}",
    );
}

/// A bare `Region::Request` leaves the client with an answer it cannot
/// attribute, so it ends the connection.
#[test]
fn an_uncorrelated_region_request_closes_the_connection() {
    let mut driver = attached_driver();
    let refused = route_frame(
        &mut driver,
        &OwnedFrame {
            kind: MessageKind::Region.as_u16(),
            body: codec::encode(&RegionToDaemonMsg::Request {
                source: felis_protocol::messages::RegionSource::Scrollback,
                ansi: false,
                session: None,
            })
            .into(),
        },
        SubscriberId::for_test(0),
    );
    assert!(
        matches!(
            refused,
            Err(ConnError::Driver(DriverError::Correlation { .. }))
        ),
        "a Region::Request with no request id must end the connection, got {refused:?}",
    );
}

/// The refusal comes from the driver's phase column rather than a
/// `match` arm in the pump, so the same rule holds for every peer that
/// reads the arm table.
#[test]
fn a_second_opener_on_an_attached_connection_is_refused_by_phase() {
    for opener in [
        SessionToDaemonMsg::Attach {
            target: AttachTarget::Id(7),
            live_only: false,
            image_regions: false,
        },
        SessionToDaemonMsg::Create {
            args: felis_protocol::messages::SpawnArgs::default(),
            image_regions: false,
        },
    ] {
        let mut driver = attached_driver();
        let refused = route_frame(
            &mut driver,
            &OwnedFrame {
                kind: MessageKind::Session.as_u16(),
                body: codec::encode(&opener).into(),
            },
            SubscriberId::for_test(0),
        );
        assert!(
            matches!(
                refused,
                Err(ConnError::Driver(DriverError::OutOfPhase {
                    phase: felis_transport::Phase::Attached,
                    ..
                }))
            ),
            "a second {} must end the connection, got {refused:?}",
            opener.variant(),
        );
    }
}

/// An unknown frame kind cannot be skipped: the next frame is only
/// meaningful if both ends agreed on this one.
#[test]
fn an_unknown_frame_kind_closes_the_connection() {
    let mut driver = attached_driver();
    let refused = route_frame(
        &mut driver,
        &OwnedFrame {
            kind: 0xBEEF,
            body: Vec::new().into(),
        },
        SubscriberId::for_test(0),
    );
    assert!(
        matches!(
            refused,
            Err(ConnError::Driver(DriverError::UnexpectedKind { .. }))
        ),
        "an unknown kind must end the connection, got {refused:?}",
    );
}

/// What `route_frame` is handed: a connection whose subscription
/// already landed, so the session surfaces are in phase.
fn attached_driver() -> DaemonDriver {
    let mut driver = ConnectionDriver::daemon();
    driver.preface_done();
    driver.handshake_done(ConnectionMode::Window);
    driver.attached();
    driver
}

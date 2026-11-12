use super::*;

use felis_protocol::preface::{PROTOCOL_MAJOR, PROTOCOL_MINOR, PrefaceError};
#[cfg(unix)]
use felis_protocol::{
    messages::{MAX_RETARGET_DESCRIPTOR_BYTES, RetargetCarrier, RetargetLanding},
    preface::{DaemonAccept, DaemonPreface},
};
#[cfg(unix)]
use felis_transport::{Endpoint, local::Listener, preface::write_daemon_preface};
#[cfg(unix)]
use tokio::io::AsyncReadExt as _;

#[test]
fn is_transient_classifies_each_variant() {
    let io = ConnectError::Io(std::io::Error::from(std::io::ErrorKind::ConnectionRefused));
    assert!(io.is_transient(), "Io must be transient");

    // `Transport` is skipped: `TransportError` is not constructable
    // from the public API.
    assert!(
        ConnectError::EofBeforeWelcome.is_transient(),
        "EofBeforeWelcome must be transient"
    );
    assert!(
        ConnectError::EofMidAttach.is_transient(),
        "EofMidAttach must be transient"
    );

    assert!(
        !ConnectError::UnexpectedKind { kind: 999 }.is_transient(),
        "UnexpectedKind is a wire-level violation, not transient"
    );
    assert!(
        !ConnectError::NotWelcome.is_transient(),
        "NotWelcome is a wire-level violation, not transient"
    );
    assert!(
        !ConnectError::Refused {
            reason: RefusalReason::Role,
            detail: String::new(),
        }
        .is_transient(),
        "a mode refusal is a verdict about this connection, not transient"
    );
    assert!(
        ConnectError::Refused {
            reason: RefusalReason::AtCapacity,
            detail: String::new(),
        }
        .is_transient(),
        "a full daemon answers differently once a peer leaves"
    );
    assert!(
        !ConnectError::MajorMismatch {
            client_major: 1,
            daemon_min: 2,
            daemon_max: 3,
        }
        .is_transient(),
        "a protocol major mismatch must fail fast, not retry"
    );
    assert!(
        !ConnectError::AttachFailed {
            reason: AttachFailure::UnknownSession,
            detail: String::new(),
        }
        .is_transient(),
        "AttachFailed is a daemon-side decision, not transient"
    );
    assert!(
        !ConnectError::UnknownPrefaceStatus {
            status: 7,
            words: [9, 9],
            client_major: PROTOCOL_MAJOR,
        }
        .is_transient(),
        "a preface refusal must fail fast whether or not we can name it"
    );
    assert!(
        !ConnectError::NotSessionAttached.is_transient(),
        "NotSessionAttached is a wire-level violation, not transient"
    );
    assert!(
        !ConnectError::MinorTooOld {
            needs: PROTOCOL_MINOR + 1,
            effective: PROTOCOL_MINOR,
            feature: "an addition this connection predates",
        }
        .is_transient(),
        "a daemon too old for a feature stays too old on a redial"
    );

    // The `Preface` split is on the inner error, which the exhaustive
    // outer match cannot force: collapsed, the retry loop would sit
    // out its whole backoff against an SSH banner.
    assert!(
        ConnectError::Preface(PrefaceExchangeError::Io(std::io::Error::from(
            std::io::ErrorKind::UnexpectedEof
        )))
        .is_transient(),
        "a carrier that died mid-preface deserves the backoff"
    );
    assert!(
        !ConnectError::Preface(PrefaceExchangeError::Preface(PrefaceError::NotFelis {
            magic: *b"SSH-",
        }))
        .is_transient(),
        "a peer that is not felis must fail fast, not retry"
    );
}

#[cfg(all(unix, feature = "daemon-tests"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_carrier_with_retry_succeeds_after_daemon_starts_late() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));

    let server_path = path.clone();
    let server_pool = pool.clone();
    let server = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        drop(serve_unix(&server_path, DaemonCaps::default(), server_pool).await);
    });

    let policy = RetryPolicy {
        initial_backoff: Duration::from_millis(50),
        max_backoff: Duration::from_millis(200),
        max_attempts: NonZeroU32::new(10).unwrap(),
    };
    let conn =
        connect_carrier_with_retry(Carrier::Local(path.as_path().into()), Offer::ops(), policy)
            .await
            .expect("retry should land once the daemon is up");
    drop(conn);
    server.abort();
}

/// A peer that accepts and says nothing has been reached, so the
/// expiry names the handshake and not the connect.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bounded_dial_names_the_phase_its_deadline_caught() {
    let tmp = private_dir();
    let path = tmp.path().join("silent.sock");
    let server = Listener::bind(&Endpoint::unix(path.clone())).expect("bind");
    let server_task = tokio::spawn(async move {
        let _stream = server.accept().await.expect("accept");
        std::future::pending::<()>().await;
    });

    let dialed = dial_bounded(
        Carrier::Local(path.as_path().into()),
        Offer::ops(),
        RemoteSpawn::Refuse,
        Duration::from_millis(200),
    )
    .await;
    let Err(err) = dialed else {
        panic!("a silent peer never finishes the handshake");
    };

    assert!(
        matches!(err, BoundedDialError::HandshakeTimedOut { .. }),
        "{err:?}"
    );
    server_task.abort();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_carrier_with_retry_does_not_retry_on_protocol_error() {
    let tmp = private_dir();
    let path = tmp.path().join("bogus.sock");
    let server = Listener::bind(&Endpoint::unix(path.clone())).expect("bind");

    let server_task = tokio::spawn(async move {
        let stream = server.accept().await.expect("accept");
        let (_read, mut write) = stream.into_split();
        // Accepted so the failure under test is the frame, not the preface.
        write_daemon_preface(
            &mut write,
            DaemonAccept::select(ClientPreface::CURRENT).expect("served"),
        )
        .await
        .expect("write accept");
        let mut writer = FrameWriter::at_build_minor(write);
        writer
            .write_frame_unchecked(&Frame {
                kind: MessageKind::Input.as_u16(),
                body: b"garbage",
            })
            .await
            .expect("write bogus frame");
        writer.flush().await.unwrap();
        // Held open: a close that won the race against the client's
        // Hello would read as a transient Io error and retry the
        // whole backoff schedule.
        std::future::pending::<()>().await;
    });

    let policy = RetryPolicy {
        initial_backoff: Duration::from_secs(60),
        max_backoff: Duration::from_secs(60),
        max_attempts: NonZeroU32::new(5).unwrap(),
    };
    let started = Instant::now();
    let result =
        connect_carrier_with_retry(Carrier::Local(path.as_path().into()), Offer::ops(), policy)
            .await;
    let elapsed = started.elapsed();
    assert!(
        matches!(
            result,
            Err(ConnectError::Driver(DriverError::OutOfPhase {
                phase: felis_transport::Phase::Handshake,
                ..
            }))
        ),
        "an Input frame before the Welcome is out of phase"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "fail-fast path should not have slept; elapsed = {elapsed:?}"
    );

    server_task.abort();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_refuses_a_daemon_of_another_protocol_major() {
    let tmp = private_dir();
    let path = tmp.path().join("skew.sock");
    let server = Listener::bind(&Endpoint::unix(path.clone())).expect("bind");

    let (min, max) = (PROTOCOL_MAJOR + 1, PROTOCOL_MAJOR + 2);
    let server_task = tokio::spawn(async move {
        let stream = server.accept().await.expect("accept");
        let (_read, mut write) = stream.into_split();
        write_daemon_preface(
            &mut write,
            DaemonPreface::Refuse {
                min_major: min,
                max_major: max,
            },
        )
        .await
        .expect("write refusal");
        std::future::pending::<()>().await;
    });

    let err = connect(&path, Offer::ops())
        .await
        .err()
        .expect("a skewed daemon must be refused, not accepted");
    match err {
        ConnectError::MajorMismatch {
            client_major,
            daemon_min,
            daemon_max,
        } => {
            assert_eq!(client_major, PROTOCOL_MAJOR);
            assert_eq!((daemon_min, daemon_max), (min, max));
        }
        other => panic!("expected MajorMismatch, got {other:?}"),
    }

    server_task.abort();
}

/// The status and its words survive into the error: they are the
/// only description of a refusal this build has no name for.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_treats_an_unknown_preface_status_as_a_refusal() {
    let tmp = private_dir();
    let path = tmp.path().join("future.sock");
    let server = Listener::bind(&Endpoint::unix(path.clone())).expect("bind");

    let server_task = tokio::spawn(async move {
        let stream = server.accept().await.expect("accept");
        let (_read, mut write) = stream.into_split();
        write_daemon_preface(
            &mut write,
            DaemonPreface::Unknown {
                status: 7,
                words: [9, 9],
            },
        )
        .await
        .expect("write refusal");
        std::future::pending::<()>().await;
    });

    let err = connect(&path, Offer::ops())
        .await
        .err()
        .expect("an unknown status must be refused, not accepted");
    server_task.abort();
    assert!(!err.is_transient(), "a refusal must not be retried");
    let rendered = err.to_string();
    match err {
        ConnectError::UnknownPrefaceStatus {
            status,
            words,
            client_major,
        } => {
            assert_eq!(status, 7);
            assert_eq!(words, [9, 9]);
            assert_eq!(client_major, PROTOCOL_MAJOR);
        }
        other => panic!("expected UnknownPrefaceStatus, got {other:?}"),
    }
    assert!(
        rendered.contains("status 7"),
        "the status word must reach the user, got {rendered:?}"
    );
}

/// A status-0 reply naming a major the client never offered is
/// corruption, not a refusal: it must fail before the `Hello`, so
/// the fake daemon reads its side to EOF and reports what arrived.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_closes_on_an_accept_naming_an_unoffered_major() {
    let tmp = private_dir();
    let path = tmp.path().join("wrong-major.sock");
    let server = Listener::bind(&Endpoint::unix(path.clone())).expect("bind");

    let server_task = tokio::spawn(async move {
        let stream = server.accept().await.expect("accept");
        let (mut read, mut write) = stream.into_split();
        felis_transport::preface::read_client_preface(&mut read)
            .await
            .expect("client preface");
        write_daemon_preface(
            &mut write,
            DaemonPreface::Accept {
                major: PROTOCOL_MAJOR + 9,
                minor: PROTOCOL_MINOR,
            },
        )
        .await
        .expect("write accept");
        let mut writer = FrameWriter::at_build_minor(write);
        let mut after_preface = Vec::new();
        read.read_to_end(&mut after_preface)
            .await
            .expect("read to the client's close");
        // A `Welcome` after the close exercises the path a lenient
        // client would have taken; the write failing is fine.
        drop(
            writer
                .send(&ConnToClientMsg::Welcome { identity: None })
                .await,
        );
        after_preface
    });

    // Bounded: a lenient client would write `Hello` and wait for
    // the `Welcome` this fake daemon withholds until the client
    // closes, and the assertions below would never run.
    let err = tokio::time::timeout(Duration::from_secs(5), connect(&path, Offer::ops()))
        .await
        .expect("the client closes on the accept, before any frame")
        .err()
        .expect("an accept naming an unoffered major must be rejected");
    assert!(!err.is_transient(), "a re-dial reproduces a bad accept");
    match err {
        ConnectError::AcceptedUnofferedMajor { offered, accepted } => {
            assert_eq!(offered, PROTOCOL_MAJOR);
            assert_eq!(accepted, PROTOCOL_MAJOR + 9);
        }
        other => panic!("expected AcceptedUnofferedMajor, got {other:?}"),
    }
    let rendered = err.to_string();
    assert!(
        rendered.contains(&format!("accepted protocol major {}", PROTOCOL_MAJOR + 9))
            && rendered.contains(&format!("offered {PROTOCOL_MAJOR}")),
        "both majors must reach the user, got {rendered:?}"
    );
    let after_preface = server_task.await.expect("fake daemon");
    assert!(
        after_preface.is_empty(),
        "no frame may follow a bad accept, got {} bytes",
        after_preface.len()
    );
}

/// The daemon-ahead direction; the client-ahead direction is pinned
/// in `felis-daemon`'s `exchange_preface` tests.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_effective_minor_is_the_lower_of_the_two_peers() {
    for (daemon_minor, want) in [
        (PROTOCOL_MINOR + 5, PROTOCOL_MINOR),
        (PROTOCOL_MINOR, PROTOCOL_MINOR),
    ] {
        let tmp = private_dir();
        let path = tmp.path().join("minor.sock");
        let server = Listener::bind(&Endpoint::unix(path.clone())).expect("bind");

        let server_task = tokio::spawn(async move {
            let stream = server.accept().await.expect("accept");
            let (mut read, mut write) = stream.into_split();
            felis_transport::preface::read_client_preface(&mut read)
                .await
                .expect("client preface");
            write_daemon_preface(
                &mut write,
                DaemonPreface::Accept {
                    major: PROTOCOL_MAJOR,
                    minor: daemon_minor,
                },
            )
            .await
            .expect("write accept");
            let mut writer = FrameWriter::at_build_minor(write);
            let mut reader = FrameReader::new(read);
            let _hello = reader.next_frame().await.expect("hello");
            writer
                .send(&ConnToClientMsg::Welcome { identity: None })
                .await
                .expect("write welcome");
            std::future::pending::<()>().await;
        });

        let conn = match connect(&path, Offer::ops()).await {
            Ok(conn) => conn,
            Err(err) => panic!("the daemon accepted this major: {err}"),
        };
        assert_eq!(conn.effective_minor, want);
        assert_eq!(
            conn.daemon_minor, daemon_minor,
            "status reports the daemon's own minor, not the negotiated one"
        );
        server_task.abort();
    }
}

/// The daemon side of the duplex never answers, so a guard that
/// failed to fire would hang instead of returning.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_by_prefix_refuses_a_prefix_no_session_id_can_start_with() {
    for bad in ["", "zz", &"a".repeat(33)] {
        let (client_side, _daemon_side) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(client_side);
        let mut conn = Connection::from_halves(
            FrameReader::new(read),
            FrameWriter::at_build_minor(write),
            ConnectionMode::Ops,
        );
        let refused = tokio::time::timeout(
            Duration::from_secs(5),
            conn.attach_by_prefix(bad.to_owned(), AttachIntent::Deliberate),
        )
        .await
        .expect("the guard answers without a round trip");
        assert!(
            matches!(refused, Err(ConnectError::InvalidSessionPrefix { .. })),
            "`{bad}` must be refused here, not encoded: {refused:?}"
        );
    }
}

/// An over-limit descriptor is refused without a byte leaving, and
/// without burning the request id it would have travelled under
/// (REQ-105a): the fake daemon echoes the id it saw in `queued`,
/// so the id the *next* request carries is observable, and a leak
/// would show up as `2`.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_over_limit_request_does_not_burn_its_request_id() {
    let tmp = private_dir();
    let path = tmp.path().join("echo-daemon.sock");
    let server = Listener::bind(&Endpoint::unix(path.clone())).expect("bind");
    let server_task = tokio::spawn(async move {
        let stream = server.accept().await.expect("accept");
        let (mut read, mut write) = stream.into_split();
        felis_transport::preface::read_client_preface(&mut read)
            .await
            .expect("client preface");
        write_daemon_preface(
            &mut write,
            DaemonPreface::Accept {
                major: PROTOCOL_MAJOR,
                minor: PROTOCOL_MINOR,
            },
        )
        .await
        .expect("write accept");
        let mut writer = FrameWriter::at_build_minor(write);
        let mut reader = FrameReader::new(read);
        let _hello = reader.next_frame().await.expect("hello");
        writer
            .send(&ConnToClientMsg::Welcome { identity: None })
            .await
            .expect("write welcome");
        let frame = reader
            .next_frame()
            .await
            .expect("read ops")
            .expect("an ops frame");
        let correlation = codec::peek_correlation(&frame.body)
            .expect("correlated")
            .expect("a request id");
        let seen = u32::try_from(correlation.request_id().expect("a request id").get())
            .expect("the first request ids fit a u32");
        writer
            .send_correlated(
                &OpsToClientMsg::Switched {
                    from: ResolvedId::Ok { id: 1 },
                    to: None,
                    queued: seen,
                    denied: None,
                },
                correlation,
            )
            .await
            .expect("write reply");
        std::future::pending::<()>().await;
    });

    let mut conn = connect(&path, Offer::ops()).await.expect("major matches");
    let id = format!("{:032x}", 1_u128);
    let over_limit = RetargetTarget {
        carrier: RetargetCarrier::Ssh {
            destination: "a".repeat(MAX_RETARGET_DESCRIPTOR_BYTES + 1),
            ssh_args: Vec::new(),
        },
        landing: RetargetLanding::Attach(id.clone()),
    };
    let refused = conn
        .retarget_window(id.clone(), over_limit, SwitchScope::Default)
        .await;
    assert!(
        matches!(
            refused,
            Err(ConnectError::Transport(TransportError::Wire(
                felis_protocol::convert::WireError::OverLimit { .. }
            )))
        ),
        "expected an OverLimit refusal, got {refused:?}"
    );

    let reply = tokio::time::timeout(
        Duration::from_secs(5),
        conn.retarget_window(
            id.clone(),
            RetargetTarget {
                carrier: RetargetCarrier::DefaultLocal,
                landing: RetargetLanding::Attach(id),
            },
            SwitchScope::Default,
        ),
    )
    .await
    .expect("the daemon answers")
    .expect("the legal descriptor goes out");
    assert_eq!(
        reply.queued, 1,
        "the refused request kept its id: the next one is still the first",
    );

    server_task.abort();
}

/// The two questions differ on exactly one answer: a full daemon is
/// worth waiting for and must never be spawned over. Every other
/// transient failure (including the daemon-restart race that dies
/// mid-handshake) still needs the spawn-and-retry path.
#[test]
fn a_refusal_is_the_one_transient_failure_no_spawn_answers() {
    assert!(
        ConnectError::Connect(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
            .may_be_a_cold_socket(),
        "a refused connect is the stale-socket case autospawn exists for"
    );
    assert!(
        !ConnectError::Io(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
            .may_be_a_cold_socket(),
        "an I/O failure that is not the dial itself decides nothing"
    );
    let full = ConnectError::Refused {
        reason: RefusalReason::AtCapacity,
        detail: String::new(),
    };
    assert!(full.is_transient() && !full.may_be_a_cold_socket());
    assert!(
        ConnectError::EofBeforeWelcome.may_be_a_cold_socket(),
        "a daemon that exited after the accept left a socket to replace"
    );
    assert!(
        ConnectError::EofMidAttach.may_be_a_cold_socket(),
        "same race, one phase later"
    );
    assert!(
        ConnectError::Preface(PrefaceExchangeError::Io(std::io::Error::from(
            std::io::ErrorKind::UnexpectedEof
        )))
        .may_be_a_cold_socket(),
        "a peer that died mid-preface is a daemon restarting"
    );
    assert!(
        !ConnectError::NotWelcome.may_be_a_cold_socket(),
        "a peer that answered wrongly is not answered by a second daemon"
    );
}

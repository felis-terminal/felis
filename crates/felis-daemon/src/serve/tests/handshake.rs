use super::*;

/// The refusal is preface bytes, not a frame: it must be readable by a peer
/// that shares no schema with this daemon (`docs/reference/ipc.md`
/// "Versioning").
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_refuses_an_unsupported_protocol_major_in_the_preface() {
    use felis_protocol::preface::{
        ClientPreface, DaemonPreface, PROTOCOL_MAJOR, SUPPORTED_MAJOR_MAX, SUPPORTED_MAJOR_MIN,
    };
    use felis_transport::preface::{read_daemon_preface, write_client_preface};

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("true");

    spawn_daemon(&path, pool, factory);

    let (mut read_half, mut write_half) = connect(&path).await.unwrap();
    let future_major = PROTOCOL_MAJOR + 1;
    write_client_preface(
        &mut write_half,
        ClientPreface {
            major: future_major,
            minor: 0,
        },
    )
    .await
    .unwrap();

    assert_eq!(
        read_daemon_preface(&mut read_half).await.unwrap(),
        DaemonPreface::Refuse {
            min_major: SUPPORTED_MAJOR_MIN,
            max_major: SUPPORTED_MAJOR_MAX,
        },
        "the refusal must name the range the daemon serves",
    );

    let mut reader = FrameReader::new(read_half);
    let after = tokio::time::timeout(Duration::from_secs(5), reader.next_frame())
        .await
        .expect("daemon should close promptly, not hang");
    assert!(
        matches!(after, Ok(None) | Err(_)),
        "daemon must close after refusing an unsupported major, got {after:?}"
    );
}

/// Non-preface bytes get no reply at all: whatever dialed speaks another
/// protocol.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_non_felis_peer_is_closed_without_a_single_reply_byte() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("true");

    spawn_daemon(&path, pool, factory);

    let (mut read_half, mut write_half) = connect(&path).await.unwrap();
    write_half.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
    write_half.flush().await.unwrap();

    let mut seen = Vec::new();
    // The read's outcome is not asserted: the daemon closes with our unread
    // bytes in its receive buffer, which some platforms report as a reset
    // rather than EOF.
    let _closed = tokio::time::timeout(Duration::from_secs(5), read_half.read_to_end(&mut seen))
        .await
        .expect("daemon should close promptly, not hang");
    assert!(
        seen.is_empty(),
        "a non-felis peer must receive nothing, got {seen:?}"
    );
}

/// Preface and `Hello` go out in one write, so the reply order is the
/// daemon's choice rather than forced by a withheld `Hello`.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_accept_reply_precedes_the_welcome_on_the_wire() {
    use felis_protocol::preface::{
        ClientPreface, DAEMON_PREFACE_LEN, DaemonPreface, PROTOCOL_MAJOR, PROTOCOL_MINOR,
    };
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("true");

    spawn_daemon(&path, pool, factory);

    let (mut read_half, mut write_half) = connect(&path).await.unwrap();

    let mut burst = Vec::from(ClientPreface::CURRENT.encode());
    FrameWriter::at_build_minor(&mut burst)
        .send(&ConnToDaemonMsg::Hello {
            mode: ConnectionMode::Window,
            pull_paced: false,
        })
        .await
        .unwrap();
    write_half.write_all(&burst).await.unwrap();
    write_half.flush().await.unwrap();

    let mut reply = [0u8; DAEMON_PREFACE_LEN];
    tokio::time::timeout(Duration::from_secs(5), read_half.read_exact(&mut reply))
        .await
        .expect("daemon should answer promptly")
        .expect("ten reply bytes");
    assert_eq!(
        DaemonPreface::decode(&reply).expect("the first bytes back are the preface"),
        DaemonPreface::Accept {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
        },
    );

    let mut reader = FrameReader::new(read_half);
    let welcome = reader.next_frame().await.unwrap().expect("welcome");
    assert!(matches!(
        codec::decode::<ConnToClientMsg>(&welcome.body).unwrap(),
        ConnToClientMsg::Welcome { .. }
    ));
}

/// The daemon clamps the client's minor rather than adopting it: a later
/// minor announces additions this build cannot encode. Pinned on
/// `exchange_preface` because the value is unobservable on the wire.
#[tokio::test]
async fn the_daemon_clamps_a_client_minor_it_sits_behind() {
    use felis_protocol::preface::{ClientPreface, DaemonPreface, PROTOCOL_MAJOR, PROTOCOL_MINOR};
    use felis_transport::preface::{read_daemon_preface, write_client_preface};

    for client_minor in [PROTOCOL_MINOR + 5, PROTOCOL_MINOR] {
        let (client, daemon) = tokio::io::duplex(64);
        let (mut client_read, mut client_write) = tokio::io::split(client);
        let (mut daemon_read, mut daemon_write) = tokio::io::split(daemon);

        write_client_preface(
            &mut client_write,
            ClientPreface {
                major: PROTOCOL_MAJOR,
                minor: client_minor,
            },
        )
        .await
        .unwrap();

        let (effective, carrier, attach_carrier) =
            exchange_bootstrap(&mut daemon_read, &mut daemon_write, None)
                .await
                .expect("the major is served");
        assert_eq!(
            carrier, None,
            "a bare client preface carries no relay block"
        );
        assert_eq!(attach_carrier, AttachCarrier::Local { client_pid: None });
        assert_eq!(
            effective, PROTOCOL_MINOR,
            "a client at minor {client_minor} must not pull this daemon past its own",
        );
        assert_eq!(
            read_daemon_preface(&mut client_read).await.unwrap(),
            DaemonPreface::Accept {
                major: PROTOCOL_MAJOR,
                minor: PROTOCOL_MINOR,
            },
        );
    }
}

/// What the daemon reads off the bootstrap bytes alone, before any frame.
async fn carrier_of(prefix: &[u8], peer_pid: Option<NonZeroU32>) -> AttachCarrier {
    use felis_protocol::preface::{ClientPreface, PROTOCOL_MAJOR, PROTOCOL_MINOR};
    use tokio::io::AsyncWriteExt as _;

    let (client, daemon) = tokio::io::duplex(4096);
    let (_client_read, mut client_write) = tokio::io::split(client);
    let (mut daemon_read, mut daemon_write) = tokio::io::split(daemon);
    let mut bytes = prefix.to_vec();
    bytes.extend_from_slice(
        &ClientPreface {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
        }
        .encode(),
    );
    client_write.write_all(&bytes).await.unwrap();
    let (_, _, carrier) = exchange_bootstrap(&mut daemon_read, &mut daemon_write, peer_pid)
        .await
        .expect("the major is served");
    carrier
}

#[tokio::test]
async fn a_decoded_relay_block_reports_its_ssh_variables() {
    let block = CarrierBlock {
        env: vec![
            entry("HOME", "/home/u"),
            entry("SSH_CONNECTION", "192.0.2.7 50122 192.0.2.1 22"),
            entry("SSH_CLIENT", "192.0.2.7 50122 22"),
        ],
    };
    assert_eq!(
        carrier_of(&block.encode().unwrap(), NonZeroU32::new(4242)).await,
        AttachCarrier::Relay {
            ssh_env: Some(SshEnv {
                connection: Some("192.0.2.7 50122 192.0.2.1 22".into()),
                client: Some("192.0.2.7 50122 22".into()),
                tty: None,
            }),
        }
    );
}

/// A relay whose environment set no SSH variable is still a relay, with
/// values known to be unset.
#[tokio::test]
async fn a_relay_block_without_ssh_variables_reports_them_unset() {
    let block = CarrierBlock {
        env: vec![entry("HOME", "/home/u")],
    };
    assert_eq!(
        carrier_of(&block.encode().unwrap(), None).await,
        AttachCarrier::Relay {
            ssh_env: Some(SshEnv::default()),
        }
    );
}

/// A block in a later format is skipped by length: the connection is
/// still a relay's, but its values are unknown.
#[tokio::test]
async fn a_skipped_relay_block_reports_unknown_ssh_variables() {
    let payload = [0x00, 0x02, 0xde, 0xad];
    let mut block = preface::CARRIER_MAGIC.to_vec();
    block.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
    block.extend_from_slice(&payload);
    assert_eq!(
        carrier_of(&block, NonZeroU32::new(4242)).await,
        AttachCarrier::Relay { ssh_env: None }
    );
}

/// The peer pid names the window only on a local connection: a relay's
/// peer is the relay process, so the relay cases above drop it.
#[tokio::test]
async fn a_bare_preface_reports_a_local_carrier_with_the_peer_pid() {
    let pid = NonZeroU32::new(4242);
    assert_eq!(
        carrier_of(&[], pid).await,
        AttachCarrier::Local { client_pid: pid }
    );
    assert_eq!(
        carrier_of(&[], None).await,
        AttachCarrier::Local { client_pid: None }
    );
}

#[cfg(unix)]
#[test]
fn a_non_utf8_ssh_variable_makes_every_value_unknown() {
    let env = vec![
        entry("SSH_CLIENT", "192.0.2.7 50122 22"),
        (b"SSH_TTY".to_vec(), vec![0x2f, 0xff]),
    ];
    assert_eq!(ssh_env(&env), None);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_frame_on_wrong_kind_is_rejected_cleanly() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));

    let serving = bind_unix(
        &path,
        DaemonCaps::default(),
        pool.clone(),
        default_session_factory(),
    )
    .expect("the test daemon binds its socket");
    let server = tokio::spawn(async move { drop(serving.await) });

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;

    let bogus = Frame {
        kind: MessageKind::Input.as_u16(),
        body: b"not-a-hello",
    };
    writer.write_frame_unchecked(&bogus).await.unwrap();
    writer.flush().await.unwrap();

    let next = tokio::time::timeout(Duration::from_secs(2), reader.next_frame())
        .await
        .expect("daemon should close");
    match next {
        Ok(None) => {}
        other => panic!("expected clean close, got {other:?}"),
    }

    server.abort();
}

/// `handle_stream` over an in-memory duplex pair: the driver must work when
/// the carrier is stdio-shaped rather than a socket.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handle_stream_serves_a_session_over_a_carrier_pair() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("read _x");

    let (client_to_daemon_w, daemon_read) = tokio::io::duplex(64 * 1024);
    let (daemon_write, client_from_daemon_r) = tokio::io::duplex(64 * 1024);

    let server_pool = pool.clone();
    let server = tokio::spawn(async move {
        handle_stream(
            daemon_read,
            daemon_write,
            DaemonCaps::default(),
            server_pool,
            factory,
        )
        .await
    });

    let (mut reader, mut writer) = framed(client_from_daemon_r, client_to_daemon_w).await;

    hello_welcome(&mut reader, &mut writer, false).await;

    create_and_attach(&mut reader, &mut writer).await;

    let mut saw_begin = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        let Ok(Ok(Some(frame))) =
            tokio::time::timeout(Duration::from_secs(1), reader.next_frame()).await
        else {
            break;
        };
        if frame.kind == MessageKind::Grid.as_u16() {
            let msg: GridMsg = codec::decode(&frame.body).unwrap();
            if matches!(msg, GridMsg::RehydrateBegin) {
                saw_begin = true;
                break;
            }
        }
    }
    assert!(
        saw_begin,
        "stdio carrier did not deliver RehydrateBegin within deadline"
    );

    drop(writer);
    drop(reader);
    drop(tokio::time::timeout(Duration::from_secs(2), server).await);
}

/// A client built with a mode this daemon predates is refused by name
/// and then closed, rather than dropped over a decode failure it could
/// not tell from a crash. The future mode is hand-encoded: every mode
/// this build ships is a base-schema one.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hello_naming_an_unknown_mode_is_refused_by_name() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("read x");

    spawn_daemon(&path, pool.clone(), factory);

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;

    // `ConnToDaemonMsg.hello` (field 1) holding `ConnHello.mode` (field 1) = 9,
    // a mode number no build defines. Hand-encoded rather than built
    // from the generated types, which cannot name a future value.
    let body = [0x0a, 0x02, 0x08, 0x09];
    writer
        .write_frame_unchecked(&Frame {
            kind: MessageKind::Conn.as_u16(),
            body: &body,
        })
        .await
        .unwrap();
    writer.flush().await.unwrap();

    let refusal = tokio::time::timeout(Duration::from_secs(5), reader.next_frame())
        .await
        .expect("the daemon answers before it closes")
        .unwrap()
        .expect("a refusal frame");
    match codec::decode::<ConnToClientMsg>(&refusal.body).unwrap() {
        ConnToClientMsg::Refused { reason, detail } => {
            assert_eq!(reason, RefusalReason::UnknownMode);
            assert!(detail.contains('9'), "the detail names the mode: {detail}");
        }
        other => panic!("expected Refused, got {other:?}"),
    }
    assert!(
        matches!(
            tokio::time::timeout(Duration::from_secs(5), reader.next_frame()).await,
            Ok(Ok(None))
        ),
        "the daemon closes after the refusal"
    );
}

/// The refusal covers a mode that postdates this build and nothing
/// else: an unreadable `Hello` that no newer client could have written
/// is corruption, and corruption is answered by a bare close.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreadable_hello_that_is_not_a_future_mode_closes_unanswered() {
    // `ConnToDaemonMsg.hello` (field 1) holding `ConnHello.mode` (field 1), in
    // shapes no honest peer writes: the UNSPECIFIED sentinel, an unknown
    // mode under a `Correlation` envelope (field 100) that the Conn
    // family reserves, and an unknown mode beside a client-bound
    // `ConnToClientMsg.welcome` (field 2) on either side of it.
    let unspecified: &[u8] = &[0x0a, 0x02, 0x08, 0x00];
    let correlated: &[u8] = &[0x0a, 0x02, 0x08, 0x09, 0xa2, 0x06, 0x02, 0x08, 0x01];
    let then_welcome: &[u8] = &[0x0a, 0x02, 0x08, 0x09, 0x12, 0x00];
    let after_welcome: &[u8] = &[0x12, 0x00, 0x0a, 0x02, 0x08, 0x09];

    for body in [unspecified, correlated, then_welcome, after_welcome] {
        let tmp = private_dir();
        let path = tmp.path().join("daemon.sock");
        let pool = Arc::new(Mutex::new(SessionPool::new()));
        let factory = shell_factory("read x");

        spawn_daemon(&path, pool.clone(), factory);

        let (read_half, write_half) = connect(&path).await.unwrap();
        let (mut reader, mut writer) = framed(read_half, write_half).await;

        writer
            .write_frame_unchecked(&Frame {
                kind: MessageKind::Conn.as_u16(),
                body,
            })
            .await
            .unwrap();
        writer.flush().await.unwrap();

        assert!(
            matches!(
                tokio::time::timeout(Duration::from_secs(5), reader.next_frame()).await,
                Ok(Ok(None))
            ),
            "a corrupt Hello is closed without a frame: {body:?}"
        );
    }
}

/// A codec error ends the connection but not the session: a wire error
/// merely unsubscribes.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_input_frame_does_not_orphan_the_session() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("read x");

    spawn_daemon(&path, pool.clone(), factory);

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;

    hello_welcome(&mut reader, &mut writer, false).await;

    let session_id = create_and_attach(&mut reader, &mut writer).await;

    // A leading `0xff` is not a valid frame for the connection's encoding.
    writer
        .write_frame_unchecked(&Frame {
            kind: MessageKind::Input.as_u16(),
            body: &[0xff, 0xff, 0xff, 0xff],
        })
        .await
        .unwrap();
    writer.flush().await.unwrap();

    let teardown_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < teardown_deadline {
        if let Ok(Ok(None) | Err(_)) =
            tokio::time::timeout(Duration::from_millis(200), reader.next_frame()).await
        {
            break;
        }
    }
    drop(writer);
    drop(reader);

    let poll_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut parked = false;
    while tokio::time::Instant::now() < poll_deadline {
        {
            let p = pool.lock().await;
            if let Some(handle) = p.get(SessionId(session_id))
                && handle.meta_snapshot().subscribers == 0
            {
                parked = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        parked,
        "session orphaned after Codec error — must stay pooled with zero subscribers",
    );
}

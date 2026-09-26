use felis_protocol::frame::Frame;

#[cfg(unix)]
use std::{num::NonZeroU32, sync::Arc};

#[cfg(unix)]
use felis_daemon::{SessionPool, serve::DaemonCaps, serve_unix};
#[cfg(unix)]
use tempfile::TempDir;

/// A socket parent must be a `0700` directory this uid owns
/// (REQ-107), and `TempDir` follows the process umask.
#[cfg(unix)]
fn private_dir() -> TempDir {
    use std::os::unix::fs::PermissionsExt as _;

    let tmp = TempDir::new().unwrap();
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    tmp
}
#[cfg(unix)]
use tokio::sync::Mutex;

#[test]
fn relay_command_builds_the_ssh_stdio_relay_invocation() {
    let cmd = relay_command("user@host", &[], RemoteSpawn::Allow);
    let std_cmd = cmd.as_std();
    assert_eq!(std_cmd.get_program(), "ssh");
    let args: Vec<&std::ffi::OsStr> = std_cmd.get_args().collect();
    assert_eq!(args, ["user@host", "felis-daemon", "relay"]);
}

#[test]
fn relay_command_appends_no_spawn_for_refused_remote_spawn() {
    let cmd = relay_command("user@host", &[], RemoteSpawn::Refuse);
    let args: Vec<&std::ffi::OsStr> = cmd.as_std().get_args().collect();
    assert_eq!(args, ["user@host", "felis-daemon", "relay", "--no-spawn"]);
}

/// The landing half of the auto-spawn matrix: a window launch and
/// a retarget's landing both mean "hold this session for me", so
/// their relay command must leave the remote daemon free to start
/// (docs/reference/cli.md "Auto-spawning").
#[test]
fn the_landing_relay_command_lets_the_remote_daemon_spawn() {
    let cmd = relay_command("user@host", &[], crate::LANDING_REMOTE_SPAWN);
    let args: Vec<&std::ffi::OsStr> = cmd.as_std().get_args().collect();
    assert_eq!(args, ["user@host", "felis-daemon", "relay"]);
}

/// ssh requires its options before the destination.
#[test]
fn relay_command_splices_ssh_args_before_the_destination() {
    let ssh_args = vec!["-p".to_string(), "2222".to_string(), "-i".to_string()];
    let cmd = relay_command("user@host", &ssh_args, RemoteSpawn::Allow);
    let std_cmd = cmd.as_std();
    let args: Vec<&std::ffi::OsStr> = std_cmd.get_args().collect();
    assert_eq!(
        args,
        ["-p", "2222", "-i", "user@host", "felis-daemon", "relay"],
    );
}

use super::*;
use std::time::Duration;

use super::{
    ConnectError, Connection,
    carrier::{
        CarrierConnection, Offer, RemoteSpawn, handshake_over, open_stdio_command, relay_command,
    },
    check_arm_minor, check_mode_minor,
    requests::{
        AttachIntent, ForceDetachReq, SessionRefusalContext, SwitchSessionReq, TagReq, admits,
    },
};
use felis_protocol::{
    ConnectionMode,
    messages::{
        AttachFailure, AttachRefusal, ConnToClientMsg, ConnToDaemonMsg, Correlation, CreateFailure,
        Directed, OpsToClientMsg, OpsToDaemonMsg, RefusalReason, RequestId, ResolvedId,
        SessionToClientMsg, SessionToDaemonMsg, StreamErrorReason, StreamId, Subject, SwitchDenied,
        SwitchScope, SwitchTarget,
    },
    preface,
};
use felis_transport::{DriverError, FrameReader, FrameWriter, PrefaceExchangeError};
use tokio::process::Command;

#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::time::Instant;

#[cfg(unix)]
use super::carrier::{
    BoundedDialError, Carrier, connect, connect_carrier_with_retry, dial_bounded,
};
#[cfg(unix)]
use felis_protocol::{
    messages::{RetargetTarget, SessionInfo, SpawnArgs},
    preface::ClientPreface,
};
#[cfg(unix)]
use felis_transport::{Delivery, Incoming, TransportError, retry::RetryPolicy};

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
fn tag_request_keeps_adds_and_removes_on_their_own_sides() {
    let msg = TagReq {
        id_prefix: "1a2b".to_owned(),
        add: vec!["work".to_owned()],
        remove: vec!["stale".to_owned()],
    }
    .take_msg();
    assert_eq!(
        msg,
        OpsToDaemonMsg::Tag {
            id_prefix: "1a2b".to_owned(),
            add: vec!["work".to_owned()],
            remove: vec!["stale".to_owned()],
        },
    );
}

#[test]
fn switch_request_carries_the_target_arm_its_caller_chose() {
    let msg = SwitchSessionReq {
        from_prefix: "aa".to_owned(),
        target: SwitchTarget::Session("bb".to_owned()),
        scope: SwitchScope::Default,
    }
    .take_msg();
    assert_eq!(
        msg,
        OpsToDaemonMsg::Switch {
            from_prefix: "aa".to_owned(),
            target: SwitchTarget::Session("bb".to_owned()),
            scope: SwitchScope::Default,
        },
    );
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

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connector_round_trips_through_a_real_daemon() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));

    let server_path = path.clone();
    let server_pool = pool.clone();
    let server = tokio::spawn(async move {
        drop(serve_unix(&server_path, DaemonCaps::default(), server_pool).await);
    });

    for _ in 0..100 {
        if path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(path.exists());

    let mut conn = connect(&path, Offer::ops()).await.unwrap();
    assert_eq!(
        conn.effective_minor,
        felis_protocol::PROTOCOL_MINOR,
        "two same-build peers settle on their shared minor",
    );
    assert_eq!(
        conn.list_sessions().await.unwrap(),
        Vec::<SessionInfo>::new()
    );

    server.abort();
}

/// The spawn and the handshake as one step, which is what a caller
/// without its own deadline does with them.
async fn connect_stdio_command(
    cmd: Command,
    offer: Offer,
) -> Result<CarrierConnection, ConnectError> {
    let (read_half, write_half) = open_stdio_command(cmd)?;
    handshake_over(read_half, write_half, offer).await
}

/// A child that will not start is not the endpoint answering:
/// `ssh` missing from `PATH` raises `NotFound` like a cold socket
/// does, and a daemon started here would serve nobody.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_stdio_command_returns_io_error_when_command_missing() {
    let cmd = Command::new("/this/binary/does/not/exist");
    let result = connect_stdio_command(cmd, Offer::ops()).await;
    let Err(err) = result else {
        panic!("a command that does not exist cannot connect");
    };
    let ConnectError::Io(io_err) = &err else {
        panic!("expected an Io error, got: {err}");
    };
    assert_eq!(io_err.kind(), std::io::ErrorKind::NotFound);
    assert!(
        !err.may_be_a_cold_socket(),
        "a child that never started says nothing about the endpoint"
    );
}

/// A child that exits without writing surfaces as a transient
/// preface IO failure, the shape of an ssh connect failure.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_stdio_command_returns_eof_when_child_does_not_handshake() {
    let cmd = Command::new("true");
    let result = connect_stdio_command(cmd, Offer::ops()).await;
    assert!(matches!(
        result,
        Err(ConnectError::Preface(PrefaceExchangeError::Io(_)))
    ));
    assert!(result.err().is_some_and(|e| e.is_transient()));
}

#[cfg(unix)]
use felis_grid::decode_row;
#[cfg(unix)]
use felis_protocol::{MessageKind, codec, messages::GridMsg};

#[cfg(unix)]
async fn await_row_containing(
    conn: &mut Connection,
    target: &str,
    deadline: tokio::time::Instant,
) -> bool {
    while tokio::time::Instant::now() < deadline {
        let Ok(Ok(Some(frame))) =
            tokio::time::timeout(Duration::from_secs(3), conn.next_frame()).await
        else {
            return false;
        };
        if frame.kind != MessageKind::Grid.as_u16() {
            continue;
        }
        let msg: GridMsg = codec::decode(&frame.body).unwrap();
        if let GridMsg::RowDelta { rows } = msg {
            let mut styles = felis_grid::StyleTable::new();
            for (_, packed_cells) in &rows {
                let decoded = decode_row(&packed_cells.0, &mut styles).unwrap();
                let row_text: String = decoded
                    .cells
                    .iter()
                    .map(|c| match &c.grapheme {
                        felis_grid::Grapheme::Ascii(b) => *b as char,
                        felis_grid::Grapheme::Char(ch) => *ch,
                        // No cluster table here; the smoke tests search ASCII only.
                        felis_grid::Grapheme::Cluster(_) => '\u{FFFD}',
                        felis_grid::Grapheme::Empty
                        | felis_grid::Grapheme::Spacer
                        | felis_grid::Grapheme::SizedSpacer => ' ',
                    })
                    .collect();
                if row_text.contains(target) {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(unix)]
async fn daemon_running(script: &'static str) -> (TempDir, PathBuf, tokio::task::JoinHandle<()>) {
    use felis_daemon::{serve::SessionFactory, serve::serve_unix_with_factory};
    use felis_pty::Command;

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory: SessionFactory = Arc::new(move |_| {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", script]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    });
    let server_path = path.clone();
    let server = tokio::spawn(async move {
        drop(serve_unix_with_factory(&server_path, DaemonCaps::default(), pool, factory).await);
    });
    for _ in 0..100 {
        if path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(path.exists());
    (tmp, path, server)
}

#[cfg(unix)]
async fn collect_search(conn: &mut Connection, stream: StreamId) -> (usize, Result<u32, String>) {
    use felis_protocol::messages::SearchToClientMsg;

    let mut items = 0usize;
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(10), conn.next_frame())
            .await
            .expect("search stream stalled")
            .expect("read")
            .expect("the stream must end with a terminal, not EOF");
        match conn
            .driver
            .classify(&frame)
            .expect("driver refused a frame")
        {
            Incoming::Control(ConnToClientMsg::End { stream_id, count }) => {
                assert_eq!(stream_id, stream, "terminal named another stream");
                return (items, Ok(count));
            }
            Incoming::Control(ConnToClientMsg::Error {
                subject: Subject::Stream(stream_id),
                detail,
                ..
            }) => {
                assert_eq!(stream_id, stream, "terminal named another stream");
                return (items, Err(detail));
            }
            Incoming::Payload(payload) if payload.kind == MessageKind::Search => {
                if let Delivery::Deliver(delivered) = conn
                    .driver
                    .decode::<SearchToClientMsg>(&payload)
                    .expect("decode")
                    && matches!(delivered.msg, SearchToClientMsg::Match { .. })
                {
                    items += 1;
                }
            }
            _ => {}
        }
    }
}

/// Every reply is matched to its own request while a search stream
/// runs on the same connection.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_connection_answers_many_verbs_while_a_stream_runs() {
    use felis_protocol::messages::{SearchOptions, SearchToDaemonMsg};

    // The `read` builtin parks the shell; `sleep` is not on the
    // sanitized PATH these tests run with.
    let (_tmp, path, server) = daemon_running("printf 'needle-alpha\\n'; read _x").await;

    let mut conn = connect(&path, Offer::ops()).await.unwrap();
    let attach = conn.create_with(SpawnArgs::default()).await.unwrap();
    let id = attach.id;
    // An `Ops` attach gets no grid burst, so the needle's arrival is
    // observed by retrying the search.
    let mut items = 0;
    let mut stream = conn.driver.open_stream().expect("the sequence is fresh");
    for attempt in 0..100 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            stream = conn.driver.open_stream().expect("the sequence is fresh");
        }
        conn.writer
            .send_correlated(
                &SearchToDaemonMsg::Query {
                    query: "needle-".to_owned(),
                    options: SearchOptions::default(),
                },
                Correlation::stream(stream),
            )
            .await
            .unwrap();
        let (found, outcome) = collect_search(&mut conn, stream).await;
        assert_eq!(outcome, Ok(u32::try_from(found).unwrap()));
        if found > 0 {
            items = found;
            break;
        }
    }
    assert!(items > 0, "the shell never printed the needle");

    let stream = conn.driver.open_stream().expect("the sequence is fresh");
    conn.writer
        .send_correlated(
            &SearchToDaemonMsg::Query {
                query: "needle-".to_owned(),
                options: SearchOptions::default(),
            },
            Correlation::stream(stream),
        )
        .await
        .unwrap();

    let listed = conn.list_sessions().await.expect("list on a live stream");
    assert!(
        listed.iter().any(|s| s.id == id),
        "the roster must list the session this connection attached"
    );
    let (resolved, tags, denied) = conn
        .set_tags(format!("{id:032x}"), vec!["marked".to_owned()], Vec::new())
        .await
        .expect("tag on a live stream");
    assert_eq!(resolved, ResolvedId::Ok { id });
    assert_eq!(tags, vec!["marked".to_owned()]);
    assert!(denied.is_none());
    let switched = conn
        .switch_session(
            format!("{id:032x}"),
            format!("{id:032x}"),
            SwitchScope::Default,
        )
        .await
        .expect("switch on a live stream");
    assert_eq!(switched.from, ResolvedId::Ok { id });
    assert_eq!(switched.to, Some(ResolvedId::Ok { id }));
    assert_eq!(switched.queued, 0);
    assert_eq!(switched.denied, None);
    let listed = conn.list_sessions().await.expect("second list");
    assert!(
        listed
            .iter()
            .any(|s| s.id == id && s.tags == vec!["marked".to_owned()]),
        "the second list must observe the tag the earlier verb set",
    );

    let (found, outcome) = collect_search(&mut conn, stream).await;
    assert!(found > 0, "the search must have found the needle");
    assert_eq!(outcome, Ok(u32::try_from(found).unwrap()));

    server.abort();
}

/// The daemon's slot is freed by the terminal its outbound pump
/// writes, which no in-process driver test can reach.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sequential_streams_free_their_slot_on_the_daemon() {
    use felis_protocol::messages::{SearchOptions, SearchToDaemonMsg};
    use felis_transport::MAX_OUTSTANDING_STREAMS;

    let (_tmp, path, server) = daemon_running("read _x").await;

    let mut conn = connect(&path, Offer::ops()).await.unwrap();
    drop(conn.create_with(SpawnArgs::default()).await.unwrap());

    for round in 0..MAX_OUTSTANDING_STREAMS + 8 {
        let stream = conn.driver.open_stream().expect("the sequence is fresh");
        conn.writer
            .send_correlated(
                &SearchToDaemonMsg::Query {
                    query: "needle-".to_owned(),
                    options: SearchOptions::default(),
                },
                Correlation::stream(stream),
            )
            .await
            .unwrap();
        let (_found, outcome) = collect_search(&mut conn, stream).await;
        assert!(
            outcome.is_ok(),
            "search #{round} was refused: {outcome:?} — the daemon never freed its slot",
        );
        assert_eq!(conn.driver.outstanding_streams(), 0, "round {round}");
    }

    server.abort();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_window_is_refused_a_mutating_verb_without_losing_the_connection() {
    let (_tmp, path, server) = daemon_running("read _x").await;

    let mut conn = connect(&path, Offer::window(false)).await.unwrap();
    let attach = conn.create_with(SpawnArgs::default()).await.unwrap();
    let id = attach.id;

    let refused = conn
        .set_tags(format!("{id:032x}"), vec!["nope".to_owned()], Vec::new())
        .await;
    assert!(
        matches!(
            refused,
            Err(ConnectError::StreamRefused {
                reason: StreamErrorReason::InvalidRequest,
                ..
            })
        ),
        "a window's Tag must be refused typed, got {refused:?}",
    );

    let listed = conn.list_sessions().await.expect("list after a refusal");
    assert!(listed.iter().any(|s| s.id == id));

    server.abort();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_a_search_mid_stream_stops_it_and_leaves_the_connection_usable() {
    use felis_protocol::messages::{SearchOptions, SearchToDaemonMsg};

    // Deep enough that the walk cannot finish inside one slice.
    let (_tmp, path, server) =
        daemon_running("i=0; while [ $i -lt 2000 ]; do echo needle-$i; i=$((i+1)); done; read _x")
            .await;

    let mut conn = connect(&path, Offer::window(false)).await.unwrap();
    conn.create_with(SpawnArgs::default()).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    assert!(
        await_row_containing(&mut conn, "needle-1999", deadline).await,
        "the shell never filled the scrollback"
    );

    let stream = conn.driver.open_stream().expect("the sequence is fresh");
    conn.writer
        .send_correlated(
            &SearchToDaemonMsg::Query {
                query: "needle-".to_owned(),
                options: SearchOptions::default(),
            },
            Correlation::stream(stream),
        )
        .await
        .unwrap();
    conn.driver.cancel_stream(stream);
    conn.writer
        .send(&ConnToDaemonMsg::Cancel { stream_id: stream })
        .await
        .unwrap();

    let (_dropped, outcome) = collect_search(&mut conn, stream).await;
    assert!(outcome.is_ok(), "a canceled stream ends clean: {outcome:?}");
    assert_eq!(
        conn.driver.classify_stream(stream),
        felis_transport::StreamClass::Terminated,
    );

    let listed = conn.list_sessions().await.expect("list after a cancel");
    assert_eq!(listed.len(), 1, "the session is still there");

    server.abort();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_huge_scrollback_search_emits_before_the_walk_completes() {
    use felis_protocol::messages::{SearchOptions, SearchToClientMsg, SearchToDaemonMsg};

    let (_tmp, path, server) =
        daemon_running("i=0; while [ $i -lt 2000 ]; do echo needle-$i; i=$((i+1)); done; read _x")
            .await;

    let mut conn = connect(&path, Offer::window(false)).await.unwrap();
    conn.create_with(SpawnArgs::default()).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    assert!(
        await_row_containing(&mut conn, "needle-1999", deadline).await,
        "the shell never filled the scrollback"
    );

    let stream = conn.driver.open_stream().expect("the sequence is fresh");
    conn.writer
        .send_correlated(
            &SearchToDaemonMsg::Query {
                query: "needle-".to_owned(),
                options: SearchOptions::default(),
            },
            Correlation::stream(stream),
        )
        .await
        .unwrap();

    loop {
        let frame = tokio::time::timeout(Duration::from_secs(15), conn.next_frame())
            .await
            .expect("no first match within the deadline")
            .expect("read")
            .expect("eof before the first match");
        match conn
            .driver
            .classify(&frame)
            .expect("driver refused a frame")
        {
            Incoming::Control(ConnToClientMsg::End { .. } | ConnToClientMsg::Error { .. }) => {
                panic!("the walk finished before shipping a single item");
            }
            Incoming::Payload(payload) if payload.kind == MessageKind::Search => {
                let Delivery::Deliver(delivered) = conn
                    .driver
                    .decode::<SearchToClientMsg>(&payload)
                    .expect("decode")
                else {
                    continue;
                };
                if matches!(delivered.msg, SearchToClientMsg::Match { .. }) {
                    break;
                }
            }
            _ => {}
        }
    }
    assert_eq!(
        conn.driver.classify_stream(stream),
        felis_transport::StreamClass::Active,
        "the stream must still be walking when its first item lands",
    );

    server.abort();
}

/// Both halves are `Window` connections: only the window burst
/// carries grid rows.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killed_client_reattaches_and_observes_prior_output() {
    use felis_daemon::{serve::SessionFactory, serve::serve_unix_with_factory};
    use felis_pty::Command;

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));

    // The `read` builtin parks the shell: a host with no FHS `/bin`
    // resolves no `sleep` under this pinned `PATH`.
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "printf reattach-ok; read _x"]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    });

    let server_path = path.clone();
    let server_pool = pool.clone();
    let server = tokio::spawn(async move {
        drop(
            serve_unix_with_factory(&server_path, DaemonCaps::default(), server_pool, factory)
                .await,
        );
    });

    for _ in 0..100 {
        if path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(path.exists());

    let mut a = connect(&path, Offer::window(false)).await.unwrap();
    let attach = a.create_with(SpawnArgs::default()).await.unwrap();
    let session_id = attach.id;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    assert!(
        await_row_containing(&mut a, "reattach-ok", deadline).await,
        "client A never saw the sentinel"
    );
    // No explicit Detach: the close-mid-loop path a hung-up window takes.
    drop(a);

    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut b = connect(&path, Offer::window(false)).await.unwrap();
    assert!(
        b.list_sessions()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session_id),
        "Ops::List did not report the surviving session"
    );
    b.attach(session_id, AttachIntent::Deliberate)
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    assert!(
        await_row_containing(&mut b, "reattach-ok", deadline).await,
        "client B's rehydration did not contain the prior shell output"
    );

    server.abort();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn destroy_session_removes_session_and_acks_existed() {
    use felis_daemon::{serve::SessionFactory, serve::serve_unix_with_factory};
    use felis_pty::Command;

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));

    // The `read` builtin parks the shell: a host with no FHS `/bin`
    // resolves no `sleep` under this pinned `PATH`.
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "read _x"]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    });

    let server_pool = pool.clone();
    let server_path = path.clone();
    let server = tokio::spawn(async move {
        drop(
            serve_unix_with_factory(&server_path, DaemonCaps::default(), server_pool, factory)
                .await,
        );
    });

    for _ in 0..100 {
        if path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let mut a = connect(&path, Offer::ops()).await.unwrap();
    let id = a.create_with(SpawnArgs::default()).await.unwrap().id;
    drop(a);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut killer = connect(&path, Offer::ops()).await.unwrap();
    let resolved = killer
        .destroy_session(felis_protocol::SessionHex(id).to_string())
        .await
        .unwrap();
    assert_eq!(
        resolved,
        ResolvedId::Ok { id },
        "session must have been found and removed"
    );
    drop(killer);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut probe = connect(&path, Offer::ops()).await.unwrap();
    assert!(
        !probe
            .list_sessions()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == id),
        "destroyed session must not reappear in the roster",
    );

    server.abort();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn destroy_session_on_missing_id_returns_existed_false() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let server_pool = pool.clone();
    let server_path = path.clone();
    let server = tokio::spawn(async move {
        drop(serve_unix(&server_path, DaemonCaps::default(), server_pool).await);
    });
    for _ in 0..100 {
        if path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let mut conn = connect(&path, Offer::ops()).await.unwrap();
    let resolved = conn.destroy_session("deadbeef".to_owned()).await.unwrap();
    assert_eq!(
        resolved,
        ResolvedId::NoMatch,
        "destroy on a non-existent prefix must resolve NoMatch, not error",
    );

    server.abort();
}

#[test]
fn is_transient_classifies_each_variant() {
    use felis_protocol::preface::{PROTOCOL_MAJOR, PrefaceError};

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

#[cfg(unix)]
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
    use felis_transport::{Endpoint, local::Listener};

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
    use felis_protocol::preface::DaemonAccept;
    use felis_protocol::{MessageKind, frame::Frame};
    use felis_transport::{Endpoint, FrameWriter, local::Listener};

    let tmp = private_dir();
    let path = tmp.path().join("bogus.sock");
    let server = Listener::bind(&Endpoint::unix(path.clone())).expect("bind");

    let server_task = tokio::spawn(async move {
        let stream = server.accept().await.expect("accept");
        let (_read, mut write) = stream.into_split();
        // Accepted so the failure under test is the frame, not the preface.
        felis_transport::preface::write_daemon_preface(
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
    use felis_protocol::preface::{DaemonPreface, PROTOCOL_MAJOR};
    use felis_transport::{Endpoint, local::Listener, preface::write_daemon_preface};

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
    use felis_protocol::preface::DaemonPreface;
    use felis_transport::{Endpoint, local::Listener, preface::write_daemon_preface};

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
            assert_eq!(client_major, felis_protocol::PROTOCOL_MAJOR);
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
    use felis_protocol::preface::{DaemonPreface, PROTOCOL_MAJOR, PROTOCOL_MINOR};
    use felis_transport::{Endpoint, local::Listener, preface::write_daemon_preface};
    use tokio::io::AsyncReadExt as _;

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
    use felis_protocol::preface::{DaemonPreface, PROTOCOL_MAJOR, PROTOCOL_MINOR};
    use felis_transport::{Endpoint, local::Listener, preface::write_daemon_preface};

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

/// The arm whose row a post-release minor would carry: nothing
/// declares it on the wire, and it stands in for the first real one.
struct FutureArm;

impl Directed for FutureArm {
    const ARMS: &'static [felis_protocol::messages::ArmMeta] =
        &[felis_protocol::messages::ArmMeta::new(
            "Future::Arm",
            felis_protocol::messages::Direction::ToDaemon,
            felis_protocol::messages::CorrelationClass::Uncorrelated,
            felis_protocol::messages::ModeSet::OPS,
        )
        .since(preface::PROTOCOL_MINOR + 1)];

    fn arm_index(&self) -> usize {
        0
    }
}

/// A client may name only the modes the negotiated minor defines.
/// Every shipped mode is a base-schema one, so the case that pins
/// the gate is a synthetic mode from one minor past this build: the
/// shape a newer client dialing an older daemon carries.
#[test]
fn a_mode_above_the_effective_minor_is_refused() {
    for mode in [
        ConnectionMode::Window,
        ConnectionMode::Ops,
        ConnectionMode::Observer,
    ] {
        assert!(
            check_mode_minor(mode.since_minor(), 0).is_ok(),
            "{mode:?} ships with the base schema"
        );
    }

    let future = preface::PROTOCOL_MINOR + 1;
    let refused = check_mode_minor(future, preface::PROTOCOL_MINOR);
    match refused {
        Err(ConnectError::MinorTooOld {
            needs, effective, ..
        }) => assert_eq!((needs, effective), (future, preface::PROTOCOL_MINOR)),
        other => panic!("expected MinorTooOld, got {other:?}"),
    }
    assert!(
        !ConnectError::MinorTooOld {
            needs: future,
            effective: preface::PROTOCOL_MINOR,
            feature: "the connection mode this client asks for",
        }
        .is_transient(),
        "a daemon that predates the mode answers the same way on every retry"
    );
}

/// The whole first-release schema is authorized at minor 0, so the
/// client-side gate is pinned by the row the first post-release
/// arm will carry: a newer client dialing an older daemon refuses
/// the verb locally rather than losing the connection at the
/// daemon's decode.
#[test]
fn an_arm_above_the_effective_minor_is_refused_locally() {
    let future = preface::PROTOCOL_MINOR + 1;
    match check_arm_minor(&FutureArm, preface::PROTOCOL_MINOR, "a future verb") {
        Err(ConnectError::MinorTooOld {
            needs,
            effective,
            feature,
        }) => assert_eq!(
            (needs, effective, feature),
            (future, preface::PROTOCOL_MINOR, "a future verb")
        ),
        other => panic!("expected MinorTooOld, got {other:?}"),
    }
    assert!(
        check_arm_minor(&FutureArm, future, "a future verb").is_ok(),
        "the gate opens at the minor the arm's own row names"
    );
    assert!(
        check_arm_minor(
            &OpsToDaemonMsg::Status,
            0,
            "the daemon's resource accounting"
        )
        .is_ok(),
        "every first-release arm is authorized at minor 0"
    );
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
    use felis_protocol::messages::{
        MAX_RETARGET_DESCRIPTOR_BYTES, RetargetCarrier, RetargetLanding,
    };
    use felis_protocol::preface::{DaemonPreface, PROTOCOL_MAJOR};
    use felis_transport::{Endpoint, local::Listener, preface::write_daemon_preface};

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
                minor: preface::PROTOCOL_MINOR,
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

#[test]
fn a_too_old_minor_is_not_transient() {
    assert!(
        !ConnectError::MinorTooOld {
            needs: preface::PROTOCOL_MINOR + 1,
            effective: preface::PROTOCOL_MINOR,
            feature: "an addition this connection predates",
        }
        .is_transient()
    );
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

#[cfg(unix)]
async fn await_session_exited(
    conn: &mut Connection,
    deadline: tokio::time::Instant,
) -> Option<u128> {
    use felis_protocol::messages::PushMsg;
    while tokio::time::Instant::now() < deadline {
        // A quiet stretch is not an answer: the deadline above is the
        // one bound, and a loaded machine running the whole suite in
        // parallel can leave the socket silent for seconds before the
        // exit propagates.
        let frame = match tokio::time::timeout(Duration::from_secs(3), conn.next_frame()).await {
            Ok(Ok(Some(frame))) => frame,
            Err(_elapsed) => continue,
            _ => return None,
        };
        if frame.kind != MessageKind::Push.as_u16() {
            continue;
        }
        if let Ok(PushMsg::SessionExited { id }) = codec::decode::<PushMsg>(&frame.body) {
            return Some(id);
        }
    }
    None
}

#[cfg(unix)]
async fn daemon_with_exiting_shell() -> (TempDir, PathBuf, tokio::task::JoinHandle<()>) {
    use felis_daemon::{serve::SessionFactory, serve::serve_unix_with_factory};
    use felis_pty::Command;

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));

    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "printf bye; exit 0"]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    });

    let server_path = path.clone();
    let server = tokio::spawn(async move {
        drop(serve_unix_with_factory(&server_path, DaemonCaps::default(), pool, factory).await);
    });

    for _ in 0..100 {
        if path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(path.exists());
    (tmp, path, server)
}

/// The daemon pushes `SessionExited` before dropping the
/// subscription (session-lifecycle.md "Telling the attached window
/// the shell exited").
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn window_client_hears_session_exited_when_shell_exits() {
    let (_tmp, path, server) = daemon_with_exiting_shell().await;

    let mut c = connect(&path, Offer::window(false)).await.unwrap();
    let attach = c.create_with(SpawnArgs::default()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    assert_eq!(
        await_session_exited(&mut c, deadline).await,
        Some(attach.id),
        "a window client must hear SessionExited for its own session on shell exit",
    );

    server.abort();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ops_attach_gets_channel_close_without_session_exited() {
    use felis_protocol::messages::PushMsg;

    let (_tmp, path, server) = daemon_with_exiting_shell().await;

    let mut c = connect(&path, Offer::ops()).await.unwrap();
    c.create_with(SpawnArgs::default()).await.unwrap();

    let mut saw_exited = false;
    let mut closed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(3), c.reader.next_frame()).await {
            Ok(Ok(Some(frame))) => {
                if frame.kind == MessageKind::Push.as_u16()
                    && matches!(
                        codec::decode::<PushMsg>(&frame.body),
                        Ok(PushMsg::SessionExited { .. })
                    )
                {
                    saw_exited = true;
                    break;
                }
            }
            Ok(Ok(None)) => {
                closed = true;
                break;
            }
            // As above: silence is not the close this test waits for,
            // so the loop simply goes round again.
            Err(_elapsed) => {}
            _ => break,
        }
    }
    assert!(
        !saw_exited,
        "a scripted Ops attach must not receive SessionExited",
    );
    assert!(
        closed,
        "the daemon must close the subscription on shell exit so the window re-dials",
    );

    server.abort();
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

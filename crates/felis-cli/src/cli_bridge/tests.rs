use std::sync::Arc;

use felis_protocol::{
    MessageKind, codec,
    messages::{RegionToClientMsg, StreamErrorReason},
};
use felis_transport::Payload;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

use super::*;
use super::{
    MAX_ID_BYTES, MAX_IN_FLIGHT, OUTPUT_ITEM_CAP,
    admission::{
        DaemonOp, Operation, Params, parse_request, resolve_bridge_cwd, spawn_args, spawn_dims,
    },
    core::{ActiveOp, ActiveState, StreamSlot, settle},
    envelope::{Body, BridgeError, envelope, error_object, stream_error_reason},
    ops::capture_row_json,
    stdio::{BridgeLine, BridgeLines, Out},
};
use crate::cli_output::ErrorKind;

#[cfg(unix)]
use std::collections::HashMap;

#[cfg(unix)]
use felis_client_core::{
    CarrierConnection, CarrierReader, CarrierWriter, Connection, Offer, Reconnector,
};
#[cfg(unix)]
use felis_protocol::messages::{OpsToDaemonMsg, SessionToDaemonMsg};
#[cfg(unix)]
use felis_transport::{FrameReader, FrameWriter};
#[cfg(unix)]
use tokio::sync::{mpsc, oneshot};

#[cfg(unix)]
use super::{
    MAX_AUXILIARY_LINKS,
    core::{Core, SessionSlot},
    link::{DaemonMsg, Link, StreamEvent, StreamItem},
};

/// An aborted task's future is dropped; a merely *detached* one's
/// keeps running.
struct AbortWitness(Arc<std::sync::atomic::AtomicBool>);

impl Drop for AbortWitness {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

struct PendingWriter;

impl tokio::io::AsyncWrite for PendingWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Pending
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

struct DelimiterFailWriter {
    wrote_body: bool,
}

impl tokio::io::AsyncWrite for DelimiterFailWriter {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if self.wrote_body {
            std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "delimiter refused",
            )))
        } else {
            self.wrote_body = true;
            std::task::Poll::Ready(Ok(buf.len()))
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn a_failed_delimiter_does_not_retire_the_terminal_id() {
    let (out, writer) = Out::start_with_writer(DelimiterFailWriter { wrote_body: false });
    let retired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let witness = Arc::clone(&retired);
    out.reserve_terminal()
        .await
        .expect("terminal capacity")
        .send("{}".to_owned(), move || {
            witness.store(true, std::sync::atomic::Ordering::SeqCst);
        });

    let _failure = out.failed().await;
    assert!(
        !retired.load(std::sync::atomic::Ordering::SeqCst),
        "a JSON body without its delimiter is not a published terminal"
    );
    writer.await.expect("the writer reports failure normally");
}

/// Stands in for a stdout whose client reads each byte as soon as it
/// is handed over: the delimiter wakes a client that reuses the id,
/// which the bridge checks with `Core::id_in_use`.
#[cfg(unix)]
struct ObservedDelimiterWriter {
    core: Arc<std::sync::OnceLock<std::sync::Weak<Core>>>,
    key: String,
    seen: std::sync::mpsc::Sender<bool>,
}

#[cfg(unix)]
impl tokio::io::AsyncWrite for ObservedDelimiterWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if buf == b"\n" {
            let core = self
                .core
                .get()
                .and_then(std::sync::Weak::upgrade)
                .expect("the core outlives its first reply");
            let key = self.key.clone();
            let seen = self.seen.clone();
            let (looked, lookup) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _sent = seen.send(core.id_in_use(&key));
                let _sent = looked.send(());
            });
            // Gives an unguarded lookup every chance to win the race.
            let _waited = lookup.recv_timeout(std::time::Duration::from_millis(200));
        }
        std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_has_read_the_delimiter_can_reuse_the_id() {
    let (near, _far) = tokio::net::UnixStream::pair().expect("a socket pair");
    let (read, write) = near.into_split();
    let anchor = Link::start(
        Connection::from_halves(
            FrameReader::new(CarrierReader::Local(read)),
            FrameWriter::at_build_minor(CarrierWriter::Local(write)),
            felis_protocol::ConnectionMode::Ops,
        ),
        "test",
    );
    let id = Value::from("reused");
    let slot = Arc::new(std::sync::OnceLock::new());
    let (seen, observed) = std::sync::mpsc::channel();
    let (out, writer) = Out::start_with_writer(ObservedDelimiterWriter {
        core: Arc::clone(&slot),
        key: id.to_string(),
        seen,
    });
    let core = Arc::new(Core {
        target: Reconnector {
            carrier: felis_client_core::Carrier::Local(felis_transport::Endpoint::unix("/unused")),
            offer: Offer::ops(),
        },
        out: out.clone(),
        anchor,
        sessions: tokio::sync::Mutex::new(HashMap::new()),
        active: std::sync::Mutex::new(HashMap::new()),
        pending_replies: std::sync::Mutex::new(HashMap::new()),
        links: Arc::new(tokio::sync::Semaphore::new(MAX_AUXILIARY_LINKS)),
    });
    let _set = slot.set(Arc::downgrade(&core));

    core.answer_immediate(&id, "{}".to_owned())
        .await
        .expect("queue the reply");
    out.close().await.expect("the writer is running");
    writer.await.expect("the writer drains");

    assert!(
        !observed.recv().expect("the client reused the id"),
        "a request sent after the delimiter was read found its id still in flight"
    );
}

/// Verify request IDs are allocated only under the writer lock.
///
/// Request IDs are positional; concurrent tasks sharing a link must not
/// interleave allocation and sending.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_id_is_allocated_only_under_the_writer_lock() {
    let (near, _far) = tokio::net::UnixStream::pair().expect("a socket pair");
    let (read, write) = near.into_split();
    let link = Link::start(
        Connection::from_halves(
            FrameReader::new(CarrierReader::Local(read)),
            FrameWriter::at_build_minor(CarrierWriter::Local(write)),
            felis_protocol::ConnectionMode::Ops,
        ),
        "test",
    );

    let writer = link.writer.lock().await;
    // Every task is at its `request` call when the barrier
    // releases, so a sequence that advanced could only have
    // advanced outside the lock.
    let gate = Arc::new(tokio::sync::Barrier::new(5));
    let verbs: Vec<_> = (0..4)
        .map(|_| {
            let link = Arc::clone(&link);
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                gate.wait().await;
                link.request(&OpsToDaemonMsg::List).await
            })
        })
        .collect();
    gate.wait().await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        link.driver.lock_or_poisoned().outstanding_requests(),
        0,
        "an id issued while the writer is held names a frame that cannot be on the wire yet"
    );

    drop(writer);
    for verb in verbs {
        verb.abort();
        drop(verb.await);
    }
}

#[test]
fn a_cancel_arriving_before_the_stream_binds_is_remembered() {
    let mut slot = StreamSlot::default();
    assert!(!slot.canceled, "a fresh operation is not canceled");
    slot.cancel();
    assert!(
        slot.canceled,
        "an unbound cancel survives until there is a stream to name",
    );
}

#[test]
fn a_request_needs_the_surface_version_an_id_and_an_op() {
    let request = parse_request(r#"{"v":1,"id":7,"op":"sessions.list"}"#)
        .unwrap_or_else(|_| panic!("a complete request parses"));
    assert_eq!(request.id, Value::from(7));
    assert_eq!(request.op, Ok(Operation::Daemon(DaemonOp::List)));
}

/// A bad line is answered, never fatal, with the id whenever one
/// could be read.
#[test]
fn a_malformed_line_reports_the_id_it_could_read() {
    let (id, _streaming, err) = parse_request(r#"{"v":1,"id":"a"}"#)
        .err()
        .unwrap_or_else(|| panic!("a request with no op is malformed"));
    assert_eq!(id, Value::from("a"));
    assert_eq!(err.kind, ErrorKind::MalformedRequest);

    let (id, _streaming, err) = parse_request("not json at all")
        .err()
        .unwrap_or_else(|| panic!("a non-JSON line is malformed"));
    assert_eq!(id, Value::Null, "an unreadable line has no id to echo");
    assert_eq!(err.kind, ErrorKind::MalformedRequest);
}

#[test]
fn a_foreign_surface_version_is_refused() {
    let (id, _streaming, err) = parse_request(r#"{"v":2,"id":1,"op":"sessions.list"}"#)
        .err()
        .unwrap_or_else(|| panic!("v2 is not this surface"));
    assert_eq!(id, Value::from(1));
    assert_eq!(err.kind, ErrorKind::MalformedRequest);
    assert!(
        err.message.contains("v1"),
        "the refusal must name what this bridge speaks: {}",
        err.message
    );
}

#[test]
fn requests_reject_unknown_fields_and_ids_that_are_not_bounded_safe_scalars() {
    for line in [
        r#"{"v":1,"id":"x","op":"sessions.list","extra":true}"#.to_owned(),
        format!(
            r#"{{"v":1,"id":"{}","op":"sessions.list"}}"#,
            "x".repeat(MAX_ID_BYTES + 1)
        ),
        // The bound is bytes, not code points: 128 three-byte
        // characters are 384 bytes on the wire.
        format!(
            r#"{{"v":1,"id":"{}","op":"sessions.list"}}"#,
            "あ".repeat(MAX_ID_BYTES)
        ),
        r#"{"v":1,"id":9007199254740992,"op":"sessions.list"}"#.to_owned(),
        r#"{"v":1,"id":1.5,"op":"sessions.list"}"#.to_owned(),
        r#"{"v":1,"id":-1,"op":"sessions.list"}"#.to_owned(),
    ] {
        let (_id, _streaming, err) = parse_request(&line)
            .err()
            .unwrap_or_else(|| panic!("the request must be refused: {line}"));
        assert_eq!(err.kind, ErrorKind::MalformedRequest, "{line}");
    }

    parse_request(r#"{"v":1,"id":9007199254740991,"op":"sessions.list"}"#).unwrap_or_else(
        |(_, _, err)| {
            panic!(
                "the inclusive safe-integer boundary is accepted: {}",
                err.message
            )
        },
    );
    assert!(
        parse_request(&format!(
            r#"{{"v":1,"id":"{}","op":"sessions.list"}}"#,
            "x".repeat(MAX_ID_BYTES)
        ))
        .is_ok(),
        "the inclusive string boundary is accepted"
    );
    assert!(
        parse_request(&format!(
            r#"{{"v":1,"id":"{}","op":"sessions.list"}}"#,
            "あ".repeat(MAX_ID_BYTES / 3)
        ))
        .is_ok(),
        "a multibyte id within the byte bound is accepted"
    );
}

/// `null` is how the published schema spells an omitted optional,
/// so the bridge must read it as absence rather than as a shape
/// error.
#[test]
fn a_null_env_reads_as_no_environment_overrides() {
    let params = serde_json::json!({ "env": Value::Null });
    assert_eq!(Params(&params).env_pairs().unwrap(), Vec::new());
}

/// A locally spawned child inherits the *bridge* process's
/// environment, never the daemon's (REQ-912a); over a relay the
/// capture describes the wrong host and stays absent.
#[test]
fn a_local_spawn_carries_the_bridge_environment_and_a_relay_none() {
    let params = serde_json::json!({});
    let local = spawn_args(
        &Params(&params),
        &felis_client_core::Carrier::Local(std::path::PathBuf::from("/tmp/felis.sock").into()),
    )
    .expect("a local spawn builds its args");
    let captured = local.env_base.expect("a local carrier captures a base");
    assert!(
        !captured.is_empty(),
        "the captured base is this process's environment"
    );

    let relay = spawn_args(
        &Params(&params),
        &felis_client_core::Carrier::Ssh {
            destination: "vm".to_owned(),
            ssh_args: Vec::new(),
        },
    )
    .expect("a relay spawn builds its args");
    assert!(relay.env_base.is_none(), "a relay sends no environment");
}

/// The wire's width, not the bridge's: a geometry past it is the
/// daemon's refusal to make, so parsing never narrows it to `u16`.
#[test]
fn a_geometry_past_sixteen_bits_parses_for_the_daemon_to_refuse() {
    let params = serde_json::json!({ "rows": 70_000, "cols": 80 });
    let dims = spawn_dims(&Params(&params))
        .expect("a whole geometry parses")
        .expect("a whole geometry names a grid");
    assert_eq!(dims.rows, 70_000);
}

#[test]
fn a_relative_spawn_cwd_is_anchored_to_the_local_bridge_directory() {
    // `/editor` is absolute on Unix but root-relative (not absolute)
    // on Windows, so anchor the test base per platform.
    let base = if cfg!(windows) {
        std::path::PathBuf::from(r"C:\editor")
    } else {
        std::path::PathBuf::from("/editor")
    };
    let expected = base.join("project").to_string_lossy().into_owned();
    assert_eq!(
        resolve_bridge_cwd(Some("project".to_owned()), true, Ok(base))
            .expect("the local directory resolves"),
        expected
    );
    assert_eq!(
        resolve_bridge_cwd(
            Some("project".to_owned()),
            false,
            Err(std::io::Error::other("not consulted"))
        )
        .expect("a relay path passes through"),
        "project"
    );
    assert!(
        resolve_bridge_cwd(
            Some("project".to_owned()),
            true,
            Err(std::io::Error::other("cwd unavailable"))
        )
        .is_err(),
        "a local path is never forwarded relative to the daemon"
    );
}

#[tokio::test]
async fn stdout_queues_bound_items_without_consuming_terminal_capacity() {
    let (out, writer) = Out::start_with_writer(PendingWriter);
    for index in 0..OUTPUT_ITEM_CAP {
        out.emit(format!("item-{index}"))
            .await
            .expect("the item budget admits its documented capacity");
    }
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(20),
            out.emit("one-too-many".to_owned())
        )
        .await
        .is_err(),
        "the next non-terminal object waits for stdout"
    );

    let mut terminals = Vec::new();
    for _ in 0..MAX_IN_FLIGHT {
        terminals.push(
            out.reserve_terminal()
                .await
                .expect("item saturation does not consume terminal slots"),
        );
    }
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), out.reserve_terminal())
            .await
            .is_err(),
        "terminal reservations are bounded by in-flight admission"
    );

    drop(terminals);
    writer.abort();
    let _joined = writer.await;
}

#[test]
fn every_object_opens_with_the_surface_version_and_the_echoed_id() {
    let id = Value::from("req-1");
    let line = envelope(&id, None, Body::Result(serde_json::json!({})));
    assert_eq!(line, r#"{"v":1,"id":"req-1","result":{}}"#);
}

/// The error object is one shape for point requests and stream
/// terminals.
#[test]
fn an_error_object_names_a_kind_and_a_message() {
    let err = BridgeError::new(ErrorKind::NoMatch, "no session matches `ff`");
    assert_eq!(
        error_object(&Value::from(3), &err),
        r#"{"v":1,"id":3,"error":{"kind":"no_match","message":"no session matches `ff`"}}"#
    );
}

/// Rows ride the shape the family's own serde form produces, not
/// a shape spelled out here.
#[test]
fn a_capture_row_carries_the_transcoded_row_object() {
    let body = codec::encode(&RegionToClientMsg::Row {
        row: -2,
        text: "hello".to_owned(),
        ansi: None,
        soft_wrap_continued: true,
    });
    let payload = Payload {
        kind: MessageKind::Region,
        body: body.into(),
        correlation: None,
        envelope_fault: None,
    };
    let value = capture_row_json(&payload)
        .unwrap_or_else(|err| panic!("a Region::Row transcodes: {}", err.message));
    assert_eq!(value["row"], Value::from(-2));
    assert_eq!(value["text"], Value::from("hello"));
    assert_eq!(value["soft_wrap_continued"], Value::from(true));
    assert!(
        value.get("Row").is_none(),
        "the frame's variant tag is not part of the row item: {value}"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_immediate_reply_holds_its_id_while_stdout_is_blocked() {
    let (near, far) = tokio::net::UnixStream::pair().expect("a socket pair");
    let (read, write) = near.into_split();
    let anchor = Link::start(
        Connection::from_halves(
            FrameReader::new(CarrierReader::Local(read)),
            FrameWriter::at_build_minor(CarrierWriter::Local(write)),
            felis_protocol::ConnectionMode::Ops,
        ),
        "test",
    );
    let (out, writer) = Out::start_with_writer(PendingWriter);
    let core = Arc::new(Core {
        target: Reconnector {
            carrier: felis_client_core::Carrier::Local(felis_transport::Endpoint::unix("/unused")),
            offer: Offer::ops(),
        },
        out,
        anchor: Arc::clone(&anchor),
        sessions: tokio::sync::Mutex::new(HashMap::new()),
        active: std::sync::Mutex::new(HashMap::new()),
        pending_replies: std::sync::Mutex::new(HashMap::new()),
        links: Arc::new(tokio::sync::Semaphore::new(MAX_AUXILIARY_LINKS)),
    });
    let id = Value::from("control");
    let loss = BridgeError::new(ErrorKind::DaemonLost, "the daemon connection was lost");
    let request =
        r#"{"v":1,"id":"control","op":"sessions.capture","params":{"session":"deadbeef"}}"#;

    core.answer_after_loss(request, &loss)
        .await
        .expect("queue the drained stream terminal");
    core.answer_after_loss(request, &loss)
        .await
        .expect("queue the duplicate refusal");
    assert!(
        core.id_in_use(&id.to_string()),
        "the id remains live until the blocked writer can publish it"
    );
    assert_eq!(
        core.pending_replies.lock_or_poisoned().get(&id.to_string()),
        Some(&2),
        "the terminal and duplicate refusal each retire only after publication"
    );

    writer.abort();
    let _joined = writer.await;
    anchor.shutdown().await;
    drop(far);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_link_remains_admitted_until_teardown_finishes() {
    let (near, far) = tokio::net::UnixStream::pair().expect("a socket pair");
    let (read, write) = near.into_split();
    let link = Link::start(
        Connection::from_halves(
            FrameReader::new(CarrierReader::Local(read)),
            FrameWriter::at_build_minor(CarrierWriter::Local(write)),
            felis_protocol::ConnectionMode::Ops,
        ),
        "test",
    );
    let links = Arc::new(tokio::sync::Semaphore::new(1));
    let permit = Arc::clone(&links)
        .acquire_owned()
        .await
        .expect("the session link is admitted");
    let mut slot = SessionSlot::vacant(permit);
    slot.link = Some(Arc::clone(&link));
    slot.users = 1;
    let (out, output_writer) = Out::start_with_writer(PendingWriter);
    let core = Arc::new(Core {
        target: Reconnector {
            carrier: felis_client_core::Carrier::Local(felis_transport::Endpoint::unix("/unused")),
            offer: Offer::ops(),
        },
        out,
        anchor: Arc::clone(&link),
        sessions: tokio::sync::Mutex::new(HashMap::from([(7, slot)])),
        active: std::sync::Mutex::new(HashMap::new()),
        pending_replies: std::sync::Mutex::new(HashMap::new()),
        links: Arc::clone(&links),
    });
    let writer = link.writer.lock().await;
    let releasing = tokio::spawn({
        let core = Arc::clone(&core);
        async move { core.release(7).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(
        Arc::clone(&links).try_acquire_owned().is_err(),
        "a replacement cannot consume the closing link's capacity"
    );

    drop(writer);
    releasing.await.expect("teardown finishes");
    let _replacement = Arc::clone(&links)
        .try_acquire_owned()
        .expect("teardown releases the permit");
    output_writer.abort();
    let _joined = output_writer.await;
    drop(far);
}

/// What a carrier's far end has already been given, read without
/// waiting: `Some` once it has seen EOF, `None` while it is open.
/// Waiting would let an admission released too early pass, by
/// blocking until the teardown it was supposed to follow.
#[cfg(unix)]
fn drained(peer: &mut std::os::unix::net::UnixStream) -> Option<Vec<u8>> {
    let mut seen = Vec::new();
    let mut buf = [0_u8; 512];
    loop {
        match std::io::Read::read(peer, &mut buf) {
            Ok(0) => return Some(seen),
            Ok(read) => seen.extend_from_slice(&buf[..read]),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return None,
            Err(err) => panic!("read the peer: {err}"),
        }
    }
}

#[cfg(unix)]
fn peer_of(stream: tokio::net::UnixStream) -> std::os::unix::net::UnixStream {
    let peer = stream.into_std().expect("the peer leaves the runtime");
    peer.set_nonblocking(true).expect("a non-blocking peer");
    peer
}

#[cfg(unix)]
fn carrier(stream: tokio::net::UnixStream) -> CarrierConnection {
    let (read, write) = stream.into_split();
    Connection::from_halves(
        FrameReader::new(CarrierReader::Local(read)),
        FrameWriter::at_build_minor(CarrierWriter::Local(write)),
        felis_protocol::ConnectionMode::Ops,
    )
}

#[cfg(unix)]
fn detach_body() -> Vec<u8> {
    codec::encode(&SessionToDaemonMsg::Detach)
}

/// A second dial that loses the race to install its link must not
/// hand its admission back before the daemon has the `Detach` and
/// the carrier is closed.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_redundant_adoption_yields_its_capacity_only_after_it_closes() {
    let (live_near, live_far) = tokio::net::UnixStream::pair().expect("a socket pair");
    let live = Link::start(carrier(live_near), "test");
    let links = Arc::new(tokio::sync::Semaphore::new(2));
    let held = Arc::clone(&links)
        .acquire_owned()
        .await
        .expect("the installed link is admitted");
    let mut slot = SessionSlot::vacant(held);
    slot.link = Some(Arc::clone(&live));
    slot.users = 1;
    let (out, output_writer) = Out::start_with_writer(PendingWriter);
    let core = Arc::new(Core {
        target: Reconnector {
            carrier: felis_client_core::Carrier::Local(felis_transport::Endpoint::unix("/unused")),
            offer: Offer::ops(),
        },
        out,
        anchor: Arc::clone(&live),
        sessions: tokio::sync::Mutex::new(HashMap::from([(7, slot)])),
        active: std::sync::Mutex::new(HashMap::new()),
        pending_replies: std::sync::Mutex::new(HashMap::new()),
        links: Arc::clone(&links),
    });

    let (spare_near, spare_far) = tokio::net::UnixStream::pair().expect("a socket pair");
    let permit = Arc::clone(&links)
        .acquire_owned()
        .await
        .expect("the redundant dial is admitted");
    let mut peer = peer_of(spare_far);

    let borrowed = core
        .adopt(7, carrier(spare_near), permit)
        .await
        .expect("adopt");
    assert!(
        Arc::ptr_eq(&borrowed, &live),
        "the installed link is what the loser borrows"
    );
    let _replacement = Arc::clone(&links)
        .try_acquire_owned()
        .expect("the redundant dial hands its admission back");
    let seen = drained(&mut peer).expect("the redundant carrier is closed");
    assert!(
        seen.ends_with(&detach_body()),
        "the daemon is told to detach before the admission comes back: {seen:?}"
    );

    output_writer.abort();
    let _joined = output_writer.await;
    drop(live_far);
}

/// Two dials that both find no link installed still leave one link
/// behind. The loser cannot replace the winner's: a replaced link
/// keeps its carrier and its daemon subscriber with nothing left
/// holding an admission for it.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_adoptions_install_one_link_and_close_the_other() {
    let (anchor_near, anchor_far) = tokio::net::UnixStream::pair().expect("a socket pair");
    let anchor = Link::start(carrier(anchor_near), "test");
    let links = Arc::new(tokio::sync::Semaphore::new(2));
    let (out, output_writer) = Out::start_with_writer(PendingWriter);
    let core = Arc::new(Core {
        target: Reconnector {
            carrier: felis_client_core::Carrier::Local(felis_transport::Endpoint::unix("/unused")),
            offer: Offer::ops(),
        },
        out,
        anchor: Arc::clone(&anchor),
        sessions: tokio::sync::Mutex::new(HashMap::new()),
        active: std::sync::Mutex::new(HashMap::new()),
        pending_replies: std::sync::Mutex::new(HashMap::new()),
        links: Arc::clone(&links),
    });

    let start = Arc::new(tokio::sync::Barrier::new(2));
    let mut peers = Vec::new();
    let mut adopting = Vec::new();
    for _ in 0..2 {
        let (near, far) = tokio::net::UnixStream::pair().expect("a socket pair");
        peers.push(peer_of(far));
        let permit = Arc::clone(&links)
            .acquire_owned()
            .await
            .expect("both dials are admitted");
        let core = Arc::clone(&core);
        let start = Arc::clone(&start);
        adopting.push(tokio::spawn(async move {
            let _both_in_flight = start.wait().await;
            core.install_or_borrow(7, carrier(near), permit).await
        }));
    }
    let mut installed = Vec::new();
    for task in adopting {
        installed.push(task.await.expect("the adoption finishes"));
    }
    assert!(
        Arc::ptr_eq(&installed[0], &installed[1]),
        "both adopters borrow the one installed link"
    );
    assert_eq!(
        core.sessions
            .lock()
            .await
            .get(&7)
            .expect("the session has a slot")
            .users,
        2,
        "both adopters are counted onto the slot they share"
    );

    let closed: Vec<Vec<u8>> = peers.iter_mut().filter_map(drained).collect();
    assert_eq!(
        closed.len(),
        1,
        "exactly one carrier is given up, and the other is the link"
    );
    assert!(
        closed[0].ends_with(&detach_body()),
        "the carrier given up is detached first: {:?}",
        closed[0]
    );
    let _replacement = Arc::clone(&links)
        .try_acquire_owned()
        .expect("the loser's admission comes back");
    assert!(
        Arc::clone(&links).try_acquire_owned().is_err(),
        "the installed link keeps the slot's admission"
    );

    output_writer.abort();
    let _joined = output_writer.await;
    drop(anchor_far);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutting_down_a_link_closes_both_carrier_halves() {
    let (near, mut far) = tokio::net::UnixStream::pair().expect("a socket pair");
    let (read, write) = near.into_split();
    let link = Link::start(
        Connection::from_halves(
            FrameReader::new(CarrierReader::Local(read)),
            FrameWriter::at_build_minor(CarrierWriter::Local(write)),
            felis_protocol::ConnectionMode::Ops,
        ),
        "test",
    );

    let (locked, ready) = oneshot::channel();
    let held_link = Arc::clone(&link);
    let held = tokio::spawn(async move {
        let _writer = held_link.writer.lock().await;
        let _ready = locked.send(());
        std::future::pending::<()>().await;
    });
    ready.await.expect("the cancellation writer holds the lock");
    link.cancel_writes.lock_or_poisoned().tasks.push(held);

    tokio::time::timeout(std::time::Duration::from_secs(1), link.shutdown())
        .await
        .expect("shutdown aborts tracked cancellation writes");
    let mut byte = [0_u8; 1];
    assert_eq!(
        tokio::io::AsyncReadExt::read(&mut far, &mut byte)
            .await
            .expect("read peer EOF"),
        0,
        "the peer observes EOF without waiting for any stdout work"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_write_failure_fails_and_clears_the_link() {
    let (near, far) = std::os::unix::net::UnixStream::pair().expect("a socket pair");
    far.shutdown(std::net::Shutdown::Read)
        .expect("stop the peer reading");
    near.set_nonblocking(true).expect("nonblocking client");
    let near = tokio::net::UnixStream::from_std(near).expect("tokio client");
    let (read, write) = near.into_split();
    let link = Link::start(
        Connection::from_halves(
            FrameReader::new(CarrierReader::Local(read)),
            FrameWriter::at_build_minor(CarrierWriter::Local(write)),
            felis_protocol::ConnectionMode::Ops,
        ),
        "test",
    );

    assert!(link.request(&OpsToDaemonMsg::List).await.is_err());
    assert!(link.is_lost(), "a write error is a terminal link state");
    assert!(
        link.registry.lock_or_poisoned().requests.is_empty(),
        "the request registered before the failed write is removed"
    );
    link.shutdown().await;
    drop(far);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_stream_buffer_backpressures_its_daemon_link() {
    let (near, far) = tokio::net::UnixStream::pair().expect("a socket pair");
    let (read, write) = near.into_split();
    let link = Link::start(
        Connection::from_halves(
            FrameReader::new(CarrierReader::Local(read)),
            FrameWriter::at_build_minor(CarrierWriter::Local(write)),
            felis_protocol::ConnectionMode::Ops,
        ),
        "test",
    );
    let (tx, mut rx) = mpsc::channel(1);
    link.registry.lock_or_poisoned().streams.insert(7, tx);
    link.deliver_stream(
        7,
        StreamEvent::Item(Box::new(StreamItem {
            msg: DaemonMsg::Region(RegionToClientMsg::RowsDone { exit_code: None }),
            payload: Payload {
                kind: MessageKind::Region,
                body: Vec::new().into(),
                correlation: None,
                envelope_fault: None,
            },
        })),
    )
    .await;

    let blocked_link = Arc::clone(&link);
    let mut blocked = tokio::spawn(async move {
        blocked_link
            .deliver_stream(7, StreamEvent::End { count: 1 })
            .await;
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), &mut blocked)
            .await
            .is_err(),
        "the next frame waits until the operation consumes one"
    );
    assert!(matches!(rx.recv().await, Some(StreamEvent::Item(_))));
    blocked
        .await
        .expect("the terminal is delivered after space opens");
    assert!(matches!(
        rx.recv().await,
        Some(StreamEvent::End { count: 1 })
    ));

    link.shutdown().await;
    drop(far);
}

/// A shutdown answers an operation that outlives the grace and
/// silences it afterwards: a daemon that never sends its terminal
/// must not hold the exit open, and the operation still owes its
/// client exactly one terminal. A live daemon answers a cancel, so
/// it cannot exercise this path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_operation_that_outlives_the_grace_is_answered_and_then_silenced() {
    let (writer, reader) = tokio::io::duplex(4096);
    let (out, writer_task) = Out::start_with_writer(writer);
    let terminal = out.reserve_terminal().await.expect("terminal slot");
    let entry = Arc::new(ActiveOp {
        state: std::sync::Mutex::new(ActiveState {
            done: false,
            terminal: Some(terminal),
        }),
        id: Value::from("cap"),
        stream: std::sync::Mutex::new(StreamSlot::default()),
        task: std::sync::Mutex::new(None),
        streaming: true,
    });

    let aborted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let witness = AbortWitness(Arc::clone(&aborted));
    *entry.task.lock_or_poisoned() = Some(tokio::spawn(async move {
        let _witness = witness;
        std::future::pending::<()>().await;
    }));

    settle(
        std::slice::from_ref(&entry),
        &BridgeError::new(ErrorKind::Canceled, "the bridge client closed stdin"),
        std::time::Duration::from_millis(20),
    )
    .await;

    let mut lines = BufReader::new(reader).lines();
    let terminal = lines
        .next_line()
        .await
        .expect("read terminal")
        .expect("the stalled stream is answered anyway");
    assert_eq!(
        terminal,
        r#"{"v":1,"id":"cap","event":"error","error":{"kind":"canceled","message":"the bridge client closed stdin"}}"#
    );

    assert!(
        aborted.load(std::sync::atomic::Ordering::SeqCst),
        "the operation is stopped, not detached to keep running"
    );

    entry
        .emit_open(&out, r#"{"v":1,"id":"cap","item":{}}"#.to_owned())
        .await
        .expect("the output remains open");
    out.close().await.expect("close output");
    writer_task.await.expect("writer task");
    assert!(
        lines.next_line().await.expect("read EOF").is_none(),
        "nothing follows a stream's terminal on stdout"
    );
}

/// A payload past its per-operation limit (REQ-105a) leaves the
/// bridge's output parseable: an error object on the request's own
/// id, naming the field and both numbers.
#[test]
fn an_over_limit_payload_renders_as_an_invalid_request_object() {
    use felis_protocol::messages::{InputMsg, MAX_PASTE_BYTES, Validate};

    let msg = InputMsg::Paste(vec![0u8; MAX_PASTE_BYTES + 1]);
    let err = BridgeError::over_limit(&Validate::validate(&msg).unwrap_err());
    assert_eq!(
        error_object(&Value::String("s1".to_owned()), &err),
        format!(
            r#"{{"v":1,"id":"s1","error":{{"kind":"invalid_request","message":"wire field Input::Paste carried {} bytes, over the {MAX_PASTE_BYTES} limit"}}}}"#,
            MAX_PASTE_BYTES + 1
        )
    );
}

/// An over-limit line is refused on its length alone, and the
/// bytes are dropped *while they arrive*, not at the newline: the
/// buffer is read mid-line, with the writer still feeding the same
/// line, since the cap's promise is about what the process holds,
/// not only what it forwards. The request behind it still parses.
#[tokio::test]
async fn an_over_limit_line_is_refused_without_buffering_it() {
    let (mut writer, reader) = tokio::io::duplex(64);
    let mut lines = BridgeLines {
        reader: BufReader::new(reader),
        cap: 8,
        len: 0,
        buf: Vec::new(),
        started: false,
    };

    writer.write_all(b"aaaaaaaaaaaaaaaa").await.unwrap();
    // Nothing terminates this line yet, so `next` parks after it
    // has eaten the 16 bytes; cancelling the future leaves the
    // partial line in the struct, which is what to inspect.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), lines.next())
            .await
            .is_err(),
        "an unterminated line must not resolve"
    );
    assert_eq!(lines.len, 16, "the length is counted past the cap");
    assert!(lines.buf.is_empty(), "the over-limit line was held");

    writer.write_all(b"\n{\"id\":1}\n").await.unwrap();
    drop(writer);

    let refused = lines.next().await.unwrap().expect("a line");
    let BridgeLine::TooLong(err) = refused else {
        panic!("an over-limit line must be refused");
    };
    assert_eq!(
        err.to_string(),
        "wire field bridge line carried 16 bytes, over the 8 limit"
    );

    let BridgeLine::Text(line) = lines.next().await.unwrap().expect("a line") else {
        panic!("the line after a refusal must still parse");
    };
    assert_eq!(line, "{\"id\":1}");
    assert!(lines.next().await.unwrap().is_none(), "EOF");
}

/// Verify an over-limit line is dropped as it is read (REQ-105a).
///
/// Memory stays bounded while a producer continues writing bytes
/// before a newline arrives.
#[tokio::test]
async fn an_over_limit_line_is_dropped_while_it_is_still_arriving() {
    use tokio::io::AsyncWriteExt as _;

    let (mut writer, reader) = tokio::io::duplex(64);
    let mut lines = BridgeLines {
        reader: BufReader::new(reader),
        cap: 8,
        len: 0,
        buf: Vec::new(),
        started: false,
    };

    writer.write_all(&[b'a'; 32]).await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), lines.next())
            .await
            .is_err(),
        "no newline has arrived, so the line cannot be complete"
    );
    assert!(
        lines.buf.len() <= lines.cap,
        "the over-limit line was buffered whole: {} bytes held",
        lines.buf.len()
    );
    assert_eq!(lines.len, 32, "the length is counted past the cap");

    writer.write_all(b"a\n").await.unwrap();
    let BridgeLine::TooLong(err) = lines.next().await.unwrap().expect("a line") else {
        panic!("an over-limit line must be refused");
    };
    assert_eq!(
        err.to_string(),
        "wire field bridge line carried 33 bytes, over the 8 limit"
    );
}

/// The refusal taxonomy is part of the output contract.
#[test]
fn every_wire_refusal_reason_has_a_surface_token() {
    for (reason, token) in [
        (StreamErrorReason::InvalidRequest, "invalid_request"),
        (StreamErrorReason::TooManyStreams, "too_many_streams"),
        (StreamErrorReason::Unavailable, "unavailable"),
        (StreamErrorReason::Internal, "internal"),
    ] {
        assert_eq!(stream_error_reason(reason), token);
    }
}

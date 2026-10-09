// `allow`, not `expect`: the same-arm matches live in the Unix-gated
// tests, so the lint does not fire on Windows and an `expect` would be
// unfulfilled there.
#![allow(clippy::match_same_arms)]

use std::time::Duration;

use felis_protocol::messages::{AttentionSource, ModifyOtherKeys, MouseProtocol, PaletteAction};
use felis_protocol::messages::{Correlation, RequestId, SearchToClientMsg, StreamId};
use felis_protocol::{MessageKind, codec};

use super::session_task::OutEvent;
use super::streaming::{
    RowEncodeCache, SubscriberStream, collect_facets, compose_diffs, publish_notifications,
    take_notifications,
};
use super::*;

// The end-to-end tests spawn POSIX shells over the Unix-domain serve loop
// with a `geteuid` peer credential, so they and their imports are
// Unix-only.
#[cfg(unix)]
use super::session_task;
#[cfg(unix)]
use super::streaming::{drain_effects, viewport_max_for};
#[cfg(unix)]
use felis_protocol::frame::Frame;
#[cfg(unix)]
use felis_protocol::messages::{PushMsg, RegionToClientMsg, StreamErrorReason, Subject};
#[cfg(unix)]
use felis_transport::local::connect;
#[cfg(unix)]
use std::num::NonZeroU64;
#[cfg(unix)]
use std::time::Instant;
#[cfg(unix)]
use tempfile::TempDir;

/// The scope-dependent numbers of a status row, flattened the way the
/// assertions below read them: absent where the arm does not carry the
/// field, and absent for an unlimited ceiling.
#[cfg(unix)]
fn max_subject_used(row: &felis_protocol::messages::ResourceReport) -> Option<u64> {
    use felis_protocol::messages::ReportScope;
    match row.scope {
        ReportScope::Daemon { .. } => None,
        ReportScope::Subject {
            max_subject_used, ..
        } => Some(max_subject_used),
    }
}

#[cfg(unix)]
fn per_subject_limit(row: &felis_protocol::messages::ResourceReport) -> Option<u64> {
    use felis_protocol::messages::ReportScope;
    match row.scope {
        ReportScope::Daemon { .. } => None,
        ReportScope::Subject {
            per_subject_limit, ..
        } => per_subject_limit.bound(),
    }
}

fn global_limit(row: &felis_protocol::messages::ResourceReport) -> Option<u64> {
    use felis_protocol::messages::ReportScope;
    match row.scope {
        ReportScope::Daemon { global_limit } | ReportScope::Subject { global_limit, .. } => {
            global_limit.bound()
        }
    }
}

#[cfg(unix)]
fn subject_kind(
    row: &felis_protocol::messages::ResourceReport,
) -> Option<felis_protocol::messages::SubjectKind> {
    use felis_protocol::messages::ReportScope;
    match row.scope {
        ReportScope::Daemon { .. } => None,
        ReportScope::Subject { subject, .. } => Some(subject),
    }
}

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
use crate::fixture_path;

use felis_pty::Command;

#[cfg(unix)]
fn shell_factory(body: &'static str) -> SessionFactory {
    Arc::new(move |_| {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", body]);
        cmd.env_clear();
        cmd.env("PATH", fixture_path());
        cmd
    })
}

#[cfg(unix)]
fn owned_session(body: &str) -> crate::pool::Session {
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", body]);
    cmd.env_clear();
    cmd.env("PATH", fixture_path());
    crate::pool::Session::from_spawned(SpawnedPty::spawn(cmd).expect("spawn /bin/sh"))
}

fn seeded_stream_into(grid: &Grid, out: &mut Vec<OutEvent>) -> SubscriberStream {
    let images = felis_grid::images::ImageStore::new(1024);
    let placements = felis_grid::images::Placements::new();
    SubscriberStream::rehydrated(ConnectionMode::Window, grid, &images, &placements, out)
        .expect("rehydrate compose")
}

fn seeded_stream(grid: &Grid) -> SubscriberStream {
    seeded_stream_into(grid, &mut Vec::new())
}

fn compose_one(
    grid: &mut Grid,
    stream: &mut SubscriberStream,
    out: &mut Vec<OutEvent>,
) -> Result<(), ConnError> {
    let mut rows = RowEncodeCache::default();
    rows.begin_cycle(1);
    compose_diffs(grid, stream, &mut rows, out)
}

fn merge_damage(grid: &mut Grid, stream: &mut SubscriberStream) {
    stream.damage.merge(grid.damage());
    grid.damage_mut().clear();
}

fn grid_msgs(out: &[OutEvent]) -> Vec<GridMsg> {
    out.iter()
        .filter_map(|ev| match ev {
            OutEvent::Grid(msg) => Some(msg.clone()),
            _ => None,
        })
        .collect()
}

fn row_text(packed_cells: &[u8]) -> String {
    let decoded = felis_grid::decode_row(packed_cells, &mut felis_grid::StyleTable::new()).unwrap();
    decoded
        .cells
        .iter()
        .map(|c| match &c.grapheme {
            felis_grid::Grapheme::Ascii(b) => *b as char,
            felis_grid::Grapheme::Char(ch) => *ch,
            felis_grid::Grapheme::Cluster(_) => '\u{FFFD}',
            felis_grid::Grapheme::Empty
            | felis_grid::Grapheme::Spacer
            | felis_grid::Grapheme::SizedSpacer => ' ',
        })
        .collect()
}

#[cfg(unix)]
async fn create_and_attach<R, W>(reader: &mut FrameReader<R>, writer: &mut FrameWriter<W>) -> u128
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    create_and_attach_with_dims(reader, writer, None).await
}

/// The create ack's full roster row, for tests that assert on more
/// than the id.
#[cfg(unix)]
async fn create_and_attach_info<R, W>(
    reader: &mut FrameReader<R>,
    writer: &mut FrameWriter<W>,
) -> SessionInfo
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    send_kind(
        writer,
        &SessionToDaemonMsg::Create {
            args: felis_protocol::messages::SpawnArgs::default(),
        },
    )
    .await;
    let created = reader.next_frame().await.unwrap().expect("session-created");
    match codec::decode::<SessionToClientMsg>(&created.body).unwrap() {
        SessionToClientMsg::Created { info } => info,
        other => panic!("expected SessionToClientMsg::Created, got {other:?}"),
    }
}

#[cfg(unix)]
async fn create_and_attach_with_dims<R, W>(
    reader: &mut FrameReader<R>,
    writer: &mut FrameWriter<W>,
    dims: Option<felis_protocol::messages::RequestedDims>,
) -> u128
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    send_kind(
        writer,
        &SessionToDaemonMsg::Create {
            args: felis_protocol::messages::SpawnArgs {
                dims,
                ..Default::default()
            },
        },
    )
    .await;
    let created = reader.next_frame().await.unwrap().expect("session-created");
    // No follow-up `Attach`: the create ack means this connection is
    // already subscribed.
    match codec::decode::<SessionToClientMsg>(&created.body).unwrap() {
        SessionToClientMsg::Created { info } => info.id,
        other => panic!("expected SessionToClientMsg::Created, got {other:?}"),
    }
}

/// Blocks until the socket file appears so a connect cannot race the bind.
#[cfg(unix)]
async fn spawn_daemon(path: &Path, pool: Arc<Mutex<SessionPool>>, factory: SessionFactory) {
    spawn_daemon_with_caps(path, pool, factory, DaemonCaps::default()).await;
}

#[cfg(unix)]
async fn spawn_daemon_with_caps(
    path: &Path,
    pool: Arc<Mutex<SessionPool>>,
    factory: SessionFactory,
    caps: DaemonCaps,
) {
    let server_path = path.to_path_buf();
    tokio::spawn(async move {
        drop(serve_unix_with_factory(&server_path, caps, pool, factory).await);
    });
    for _ in 0..100 {
        if path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(path.exists());
}

/// The order matters: a `FrameReader` built first would read ahead past the
/// daemon's 10-byte preface reply.
#[cfg(unix)]
async fn framed<R, W>(mut read_half: R, mut write_half: W) -> (FrameReader<R>, FrameWriter<W>)
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use felis_protocol::preface::{ClientPreface, DaemonPreface, PROTOCOL_MAJOR, PROTOCOL_MINOR};
    use felis_transport::preface::{read_daemon_preface, write_client_preface};

    write_client_preface(&mut write_half, ClientPreface::CURRENT)
        .await
        .unwrap();
    assert_eq!(
        read_daemon_preface(&mut read_half).await.unwrap(),
        DaemonPreface::Accept {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
        },
    );
    (
        FrameReader::new(read_half),
        FrameWriter::at_build_minor(write_half),
    )
}

#[cfg(unix)]
async fn hello_welcome<R, W>(
    reader: &mut FrameReader<R>,
    writer: &mut FrameWriter<W>,
    pull_paced: bool,
) where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    hello_welcome_as(reader, writer, ConnectionMode::Window, pull_paced).await;
}

#[cfg(unix)]
async fn hello_welcome_as<R, W>(
    reader: &mut FrameReader<R>,
    writer: &mut FrameWriter<W>,
    mode: ConnectionMode,
    pull_paced: bool,
) where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    send_kind(writer, &ConnToDaemonMsg::Hello { mode, pull_paced }).await;
    let welcome = reader.next_frame().await.unwrap().expect("welcome");
    match codec::decode::<ConnToClientMsg>(&welcome.body).unwrap() {
        ConnToClientMsg::Welcome { .. } => {}
        other => panic!("expected Welcome, got {other:?}"),
    }
}

#[cfg(unix)]
async fn send_kind<W, M>(writer: &mut FrameWriter<W>, msg: &M)
where
    W: tokio::io::AsyncWrite + Unpin,
    M: codec::WireCodec + felis_protocol::MinorGated + Directed + Clone + Sync,
{
    writer.send(msg).await.unwrap();
}

#[cfg(unix)]
async fn send_input<W>(writer: &mut FrameWriter<W>, msg: &InputMsg)
where
    W: tokio::io::AsyncWrite + Unpin,
{
    send_kind(writer, msg).await;
}

#[cfg(unix)]
async fn send_request<W, M>(writer: &mut FrameWriter<W>, msg: &M, request: u64)
where
    W: tokio::io::AsyncWrite + Unpin,
    M: codec::Correlated + felis_protocol::MinorGated + Directed + Clone + Sync,
{
    writer
        .send_correlated(
            msg,
            Correlation::request(RequestId::new(request).expect("non-zero request id")),
        )
        .await
        .unwrap();
}

#[cfg(unix)]
async fn send_stream<W, M>(writer: &mut FrameWriter<W>, msg: &M, stream: u64)
where
    W: tokio::io::AsyncWrite + Unpin,
    M: codec::Correlated + felis_protocol::MinorGated + Directed + Clone + Sync,
{
    writer
        .send_correlated(
            msg,
            Correlation::stream(StreamId::new(stream).expect("non-zero stream id")),
        )
        .await
        .unwrap();
}

#[cfg(unix)]
async fn attach_existing<R, W>(
    reader: &mut FrameReader<R>,
    writer: &mut FrameWriter<W>,
    id: u128,
) -> Vec<String>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    send_kind(
        writer,
        &SessionToDaemonMsg::Attach {
            target: AttachTarget::Id(id),
            live_only: false,
        },
    )
    .await;
    let ack = reader.next_frame().await.unwrap().expect("attached");
    assert!(matches!(
        codec::decode::<SessionToClientMsg>(&ack.body).unwrap(),
        SessionToClientMsg::Attached { .. }
    ));
    let mut rows = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "rehydrate burst did not end within deadline"
        );
        let frame = tokio::time::timeout(Duration::from_secs(2), reader.next_frame())
            .await
            .expect("read timed out")
            .unwrap()
            .expect("frame");
        if frame.kind != MessageKind::Grid.as_u16() {
            continue;
        }
        match codec::decode::<GridMsg>(&frame.body).unwrap() {
            GridMsg::RowDelta { rows: batch } => {
                rows.extend(batch.iter().map(|(_, body)| row_text(&body.0)));
            }
            GridMsg::RehydrateEnd => break,
            _ => {}
        }
    }
    rows
}

#[cfg(unix)]
async fn expect_row_containing<R>(reader: &mut FrameReader<R>, needle: &str)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline {
        let frame = match tokio::time::timeout(Duration::from_secs(5), reader.next_frame()).await {
            Ok(Ok(Some(f))) => f,
            Ok(Err(e)) => panic!("read: {e:?}"),
            Ok(Ok(None)) | Err(_) => break,
        };
        if frame.kind != MessageKind::Grid.as_u16() {
            continue;
        }
        match codec::decode::<GridMsg>(&frame.body).unwrap() {
            GridMsg::RowDelta { rows }
                if rows
                    .iter()
                    .any(|(_, body)| row_text(&body.0).contains(needle)) =>
            {
                return;
            }
            _ => {}
        }
    }
    panic!("never saw {needle:?} land on a row");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streams_pty_output_through_the_grid_to_the_wire() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("printf hello-felis; sleep 0.5");

    spawn_daemon(&path, pool, factory).await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;

    hello_welcome(&mut reader, &mut writer, false).await;

    create_and_attach(&mut reader, &mut writer).await;

    expect_row_containing(&mut reader, "hello-felis").await;
}

/// Text written after a placement in the same write lands where kitty
/// moved the cursor: right of the image, on its last row.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn text_after_a_placement_in_one_write_lands_right_of_the_image() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory(
        "printf 'top\\n\\033_Ga=T,f=24,s=1,v=1,c=2,r=2;AAAA\\033\\\\AFTER'; sleep 0.5",
    );
    spawn_daemon(&path, pool, factory).await;
    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    create_and_attach(&mut reader, &mut writer).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let (row, col) = loop {
        assert!(tokio::time::Instant::now() < deadline, "AFTER never landed");
        let frame = tokio::time::timeout(Duration::from_secs(5), reader.next_frame())
            .await
            .expect("read timed out")
            .unwrap()
            .expect("frame");
        if frame.kind != MessageKind::Grid.as_u16() {
            continue;
        }
        if let GridMsg::RowDelta { rows } = codec::decode::<GridMsg>(&frame.body).unwrap()
            && let Some(hit) = rows
                .iter()
                .find_map(|(i, body)| row_text(&body.0).find("AFTER").map(|c| (*i, c)))
        {
            break hit;
        }
    };
    assert_eq!((row, col), (2, 2));
}

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

    spawn_daemon(&path, pool, factory).await;

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

    spawn_daemon(&path, pool, factory).await;

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

    spawn_daemon(&path, pool, factory).await;

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

        let (effective, carrier) = exchange_bootstrap(&mut daemon_read, &mut daemon_write)
            .await
            .expect("the major is served");
        assert_eq!(
            carrier, None,
            "a bare client preface carries no relay block"
        );
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

/// The daemon hands region bytes back rather than spawning the argv: the
/// pipe target belongs to the requester's host.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_region_request_ships_the_bytes_back_and_spawns_nothing() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let probe = Arc::clone(&pool);
    let factory = shell_factory("read _x");

    spawn_daemon(&path, pool, factory).await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;

    hello_welcome(&mut reader, &mut writer, false).await;
    let _host = create_and_attach(&mut reader, &mut writer).await;

    send_request(
        &mut writer,
        &RegionToDaemonMsg::Request {
            source: felis_protocol::messages::RegionSource::Visible,
            ansi: false,
        },
        1,
    )
    .await;

    loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), reader.next_frame())
            .await
            .expect("the region reply should arrive")
            .unwrap()
            .expect("frame");
        if frame.kind != MessageKind::Region.as_u16() {
            continue;
        }
        if let RegionToClientMsg::Reply { .. } = codec::decode(&frame.body).unwrap() {
            break;
        }
    }
    assert_eq!(
        probe.lock().await.len(),
        1,
        "the daemon must not spawn a transient for a client-side sink",
    );
}

/// One `Attention` per cycle however many BELs arrived.
#[test]
fn collect_facets_coalesces_bel_bytes_into_one_bell() {
    let mut grid = Grid::new(2, 8);
    let mut parser = felis_vt::Parser::new();
    parser.advance(&mut grid, b"\x07\x07\x07");

    let bells = collect_facets(&mut grid)
        .into_iter()
        .filter(|m| {
            matches!(
                m,
                GridMsg::Attention {
                    source: AttentionSource::Bell
                }
            )
        })
        .count();
    assert_eq!(
        bells, 1,
        "exactly one Bell per cycle even with multiple BEL bytes",
    );

    assert!(
        collect_facets(&mut grid)
            .iter()
            .all(|m| !matches!(m, GridMsg::Attention { .. })),
        "Bell drained on cycle 1 — must not re-fire on quiescent cycle 2",
    );
}

/// Pins the fan-out contract in docs/reference/protocols/notifications.md.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fan_out_publishes_decoded_notification_to_subscribers() {
    use felis_protocol::messages::Urgency;

    let pool = SessionPool::new();
    let mut rx = pool.subscribe_notifications();
    let hub = pool.notify_hub();

    let mut grid = Grid::new(2, 20);
    let mut parser = felis_vt::Parser::new();
    parser.advance(&mut grid, b"\x1b]99;u=2;Build failed\x1b\\");

    let id = SessionId(0xABCD);
    let latest = take_notifications(&mut grid)
        .and_then(|taken| publish_notifications(&hub, id, taken, false))
        .expect("a decoded notification must publish and be returned for the meta snapshot");
    assert_eq!(latest.title.as_deref(), Some("Build failed"));
    assert_eq!(latest.urgency, Urgency::Critical);

    let ev = tokio::time::timeout(Duration::from_millis(200), rx.recv())
        .await
        .expect("recv did not time out")
        .expect("subscriber received an event");
    match ev {
        NotifyToClientMsg::Event {
            session_id,
            notification,
            attached,
            ..
        } => {
            assert_eq!(session_id, 0xABCD);
            assert_eq!(notification.title.as_deref(), Some("Build failed"));
            assert_eq!(notification.body, "");
            assert_eq!(notification.urgency, Urgency::Critical);
            assert!(!attached, "the parked drain path sets attached=false");
        }
        other => panic!("expected NotifyToClientMsg::Event, got {other:?}"),
    }

    assert!(take_notifications(&mut grid).is_none());
}

/// `ModeFlags` ships only on drift against the last value shipped to this
/// subscriber.
#[test]
fn compose_diffs_emits_mode_flags_on_drift_only() {
    let mut grid = Grid::new(2, 8);
    let mut parser = felis_vt::Parser::new();
    // modifyOtherKeys must survive the diff path so the encoder can
    // disambiguate Shift+Enter; DECSCNM reaches pixels only via this
    // snapshot.
    parser.advance(&mut grid, b"\x1b[?2004h\x1b[?1049h\x1b[>4;2m\x1b[?5h");

    let mut stream = seeded_stream(&grid);
    stream.diff.last_mode_flags = felis_grid::ModeSnapshot::default();
    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");

    let payload = grid_msgs(&out)
        .into_iter()
        .find(|m| matches!(m, GridMsg::ModeFlags { .. }))
        .expect("ModeFlags emitted on drift");
    let GridMsg::ModeFlags {
        bracketed_paste,
        alt_screen,
        mouse_protocol,
        application_cursor,
        modify_other_keys,
        application_keypad,
        win32_input_mode,
        reverse_video,
    } = payload
    else {
        unreachable!()
    };
    assert!(bracketed_paste, "?2004h flipped bracketed_paste on");
    assert!(alt_screen, "?1049h flipped alt_screen on");
    assert_eq!(mouse_protocol, MouseProtocol::Off, "no mouse mode set");
    assert!(!application_cursor, "DECCKM not toggled in this fixture");
    assert_eq!(
        modify_other_keys,
        ModifyOtherKeys::Level2,
        "CSI > 4 ; 2 m reached the wire"
    );
    assert!(!application_keypad, "DECKPAM not toggled in this fixture");
    assert!(!win32_input_mode, "?9001 not toggled in this fixture");
    assert!(reverse_video, "?5h reached the wire");

    let mut out2 = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out2).expect("compose ok");
    assert!(
        grid_msgs(&out2)
            .iter()
            .all(|m| !matches!(m, GridMsg::ModeFlags { .. })),
        "ModeFlags must not re-fire when nothing changed",
    );
}

/// DECSCNM lives only in the mode snapshot (no row byte carries it), so a
/// rehydrate must ship it.
#[test]
fn rehydrate_ships_the_active_reverse_video_state() {
    let mut grid = Grid::new(2, 8);
    felis_vt::Parser::new().advance(&mut grid, b"\x1b[?5hhello");

    let images = felis_grid::images::ImageStore::new(1024);
    let placements = felis_grid::images::Placements::new();
    let mut out = Vec::new();
    SubscriberStream::rehydrated(
        ConnectionMode::Window,
        &grid,
        &images,
        &placements,
        &mut out,
    )
    .expect("rehydrate compose");

    let flags = grid_msgs(&out)
        .into_iter()
        .find(|m| matches!(m, GridMsg::ModeFlags { .. }))
        .expect("rehydrate ships a ModeFlags snapshot");
    assert!(
        matches!(
            flags,
            GridMsg::ModeFlags {
                reverse_video: true,
                ..
            }
        ),
        "attaching to a reversed session must arrive reversed",
    );
}

/// A cell names a palette index, never a color, so `OSC 4`/`OSC 104` reach
/// the client only as palette facets.
#[test]
fn collect_facets_ships_palette_sets_and_the_whole_table_reset() {
    let mut grid = Grid::new(2, 8);
    let mut parser = felis_vt::Parser::new();
    parser.advance(&mut grid, b"\x1b]4;1;#ff0000;2;#00ff00\x1b\\");
    assert_eq!(
        collect_facets(&mut grid)
            .into_iter()
            .filter(|m| matches!(m, GridMsg::PaletteColor { .. } | GridMsg::PaletteResetAll))
            .collect::<Vec<_>>(),
        vec![
            GridMsg::PaletteColor {
                index: 1,
                action: PaletteAction::Set {
                    rgb: (0xFF, 0x00, 0x00),
                },
            },
            GridMsg::PaletteColor {
                index: 2,
                action: PaletteAction::Set {
                    rgb: (0x00, 0xFF, 0x00),
                },
            },
        ],
    );

    parser.advance(&mut grid, b"\x1b]104;1\x1b\\");
    assert!(collect_facets(&mut grid).contains(&GridMsg::PaletteColor {
        index: 1,
        action: PaletteAction::Reset,
    }));
    parser.advance(&mut grid, b"\x1b]104\x1b\\");
    assert!(collect_facets(&mut grid).contains(&GridMsg::PaletteResetAll));

    assert!(
        collect_facets(&mut grid).is_empty(),
        "palette facets must not re-fire when nothing changed",
    );
}

/// The burst has no other carrier for a palette entry.
#[test]
fn rehydrate_replays_the_palette_override_layer_ascending() {
    let mut grid = Grid::new(2, 8);
    felis_vt::Parser::new().advance(
        &mut grid,
        b"\x1b]4;200;#010203;7;#040506;33;#070809\x1b\\\x1b]104;33\x1b\\",
    );

    let images = felis_grid::images::ImageStore::new(1024);
    let placements = felis_grid::images::Placements::new();
    let mut out = Vec::new();
    SubscriberStream::rehydrated(
        ConnectionMode::Window,
        &grid,
        &images,
        &placements,
        &mut out,
    )
    .expect("rehydrate compose");

    assert_eq!(
        grid_msgs(&out)
            .into_iter()
            .filter(|m| matches!(m, GridMsg::PaletteColor { .. } | GridMsg::PaletteResetAll))
            .collect::<Vec<_>>(),
        vec![
            GridMsg::PaletteColor {
                index: 7,
                action: PaletteAction::Set {
                    rgb: (0x04, 0x05, 0x06),
                },
            },
            GridMsg::PaletteColor {
                index: 200,
                action: PaletteAction::Set {
                    rgb: (0x01, 0x02, 0x03),
                },
            },
        ],
    );
}

/// Registry entries must precede any `RowDelta` that references them: the
/// shadow resolves ids against tables already applied.
#[test]
fn compose_diffs_ships_new_hyperlinks_and_clusters_before_row_deltas() {
    let mut grid = Grid::new(1, 8);
    let mut parser = felis_vt::Parser::new();
    let mut stream = seeded_stream(&grid);
    parser.advance(
        &mut grid,
        b"\x1b]8;;https://example.com\x07a\x1b]8;;\x07e\xcc\x81",
    );
    merge_damage(&mut grid, &mut stream);

    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");

    let msgs = grid_msgs(&out);
    let position = |pred: fn(&GridMsg) -> bool, what: &str| {
        msgs.iter()
            .position(pred)
            .unwrap_or_else(|| panic!("no {what} in {msgs:?}"))
    };
    let link = position(|m| matches!(m, GridMsg::Hyperlink { .. }), "Hyperlink");
    let cluster = position(|m| matches!(m, GridMsg::Cluster { .. }), "Cluster");
    let row = position(|m| matches!(m, GridMsg::RowDelta { .. }), "RowDelta");
    assert!(
        link < row,
        "Hyperlink at {link} must precede RowDelta at {row}"
    );
    assert!(
        cluster < row,
        "Cluster at {cluster} must precede RowDelta at {row}",
    );
}

fn grid_with_scrolled_out_clusters() -> Grid {
    let mut grid = Grid::new(2, 8);
    felis_vt::Parser::new().advance(
        &mut grid,
        "a\u{0301}\r\nb\u{0301}\r\nc\u{0301}\r\nd\u{0301}".as_bytes(),
    );
    assert_eq!(grid.cluster_count(), 4, "one interned cluster per line");
    assert!(
        !grid.scrollback().is_empty(),
        "the first lines scrolled off"
    );
    grid
}

fn cluster_ids(out: &[OutEvent]) -> Vec<u32> {
    grid_msgs(out)
        .into_iter()
        .filter_map(|msg| match msg {
            GridMsg::Cluster { id, .. } => Some(id),
            _ => None,
        })
        .collect()
}

fn row_delta_at(out: &[OutEvent]) -> usize {
    grid_msgs(out)
        .iter()
        .position(|m| matches!(m, GridMsg::RowDelta { .. }))
        .expect("a composed burst carries a RowDelta")
}

/// The attach burst carries only the registry entries its rows name.
#[test]
fn rehydrate_ships_only_the_clusters_its_visible_rows_reference() {
    let grid = grid_with_scrolled_out_clusters();
    let mut out = Vec::new();
    let _stream = seeded_stream_into(&grid, &mut out);

    assert_eq!(
        cluster_ids(&out),
        vec![3, 4],
        "the scrollback-only clusters 1 and 2 stay out of the burst",
    );
    let last_cluster = grid_msgs(&out)
        .iter()
        .rposition(|m| matches!(m, GridMsg::Cluster { .. }))
        .expect("the visible clusters ship");
    assert!(
        last_cluster < row_delta_at(&out),
        "every entry a row references still precedes that row",
    );
}

/// Later compose cycles drain the unreferenced tail, so the client's table
/// converges on the daemon's.
#[test]
fn the_registry_tail_the_burst_skipped_backfills_on_later_cycles() {
    let mut grid = grid_with_scrolled_out_clusters();
    let mut burst = Vec::new();
    let mut stream = seeded_stream_into(&grid, &mut burst);

    let mut out = Vec::new();
    dirty_a_row(&mut grid, &mut stream);
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");
    assert_eq!(
        cluster_ids(&out),
        vec![1, 2],
        "the scrollback-only entries follow, lowest id first",
    );

    let mut quiet = Vec::new();
    dirty_a_row(&mut grid, &mut stream);
    compose_one(&mut grid, &mut stream, &mut quiet).expect("compose ok");
    assert!(
        cluster_ids(&quiet).is_empty(),
        "a caught-up subscriber gets no repeat",
    );
}

fn dirty_a_row(grid: &mut Grid, stream: &mut SubscriberStream) {
    felis_vt::Parser::new().advance(grid, b"x");
    merge_damage(grid, stream);
}

/// The drain rides cycles that already carry content: a frame asks for a
/// repaint, so a drain on quiescent cycles would repaint an unchanged
/// screen once per 256 entries.
#[test]
fn the_registry_drain_does_not_manufacture_a_frame_on_a_quiet_cycle() {
    let mut grid = grid_with_scrolled_out_clusters();
    let mut burst = Vec::new();
    let mut stream = seeded_stream_into(&grid, &mut burst);
    assert_eq!(cluster_ids(&burst), vec![3, 4], "burst precondition");

    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");
    assert!(
        out.is_empty(),
        "nothing changed on screen, so the cycle emits nothing: {out:?}",
    );

    dirty_a_row(&mut grid, &mut stream);
    let mut busy = Vec::new();
    compose_one(&mut grid, &mut stream, &mut busy).expect("compose ok");
    assert_eq!(cluster_ids(&busy), vec![1, 2]);
}

/// An `Ops` connection decodes no row payload, so it gets neither rows nor
/// registry entries; shipping them would be dropped traffic plus a per-cell
/// handle walk over every dirty row forever.
#[test]
fn an_ops_subscriber_is_sent_neither_rows_nor_registry_entries() {
    let mut grid = grid_with_scrolled_out_clusters();
    let images = felis_grid::images::ImageStore::new(1024);
    let placements = felis_grid::images::Placements::new();
    let mut burst = Vec::new();
    let mut stream =
        SubscriberStream::rehydrated(ConnectionMode::Ops, &grid, &images, &placements, &mut burst)
            .expect("rehydrate compose");
    assert!(
        cluster_ids(&burst).is_empty(),
        "no entries in the ops burst"
    );

    felis_vt::Parser::new().advance(&mut grid, "e\u{0301}".as_bytes());
    merge_damage(&mut grid, &mut stream);
    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");
    assert!(
        grid_msgs(&out).iter().all(|m| !matches!(
            m,
            GridMsg::Cluster { .. } | GridMsg::Hyperlink { .. } | GridMsg::RowDelta { .. }
        )),
        "an ops subscriber receives no rows, no referenced entries, and no drain: {out:?}",
    );
}

/// Browsing back composes rows naming ids the visible-first burst skipped;
/// a tail cursor would report them sent, so the sent-set must know they are
/// holes.
#[test]
fn browsing_into_scrollback_ships_the_entries_those_rows_name_first() {
    let mut grid = grid_with_scrolled_out_clusters();
    let mut burst = Vec::new();
    let mut stream = seeded_stream_into(&grid, &mut burst);
    assert_eq!(cluster_ids(&burst), vec![3, 4], "burst precondition");

    stream.diff.viewport = 1;
    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");

    let msgs = grid_msgs(&out);
    let entry = msgs
        .iter()
        .position(|m| matches!(m, GridMsg::Cluster { id: 2, .. }))
        .expect("the scrollback row's cluster must ship for the composed view");
    assert!(
        entry < row_delta_at(&out),
        "Cluster at {entry} must precede the composed RowDelta at {}",
        row_delta_at(&out),
    );
}

/// The cursor counts absolute ordinals minus `prompt_marks_pruned`, so a
/// front-prune cannot skip a mark the subscriber never received.
#[test]
fn compose_diffs_streams_marks_across_a_front_prune_without_skipping() {
    let mut grid = Grid::new(2, 8);
    let mut parser = felis_vt::Parser::new();
    parser.advance(&mut grid, b"\x1b[1;1H\x1b]133;A\x07");
    let mut stream = seeded_stream(&grid);
    assert_eq!(stream.diff.prompt_marks_sent, 1);

    for _ in 0..felis_grid::DEFAULT_SCROLLBACK_ROWS + 5 {
        parser.advance(&mut grid, b"\r\n");
    }
    assert_eq!(grid.prompt_marks(), []);
    assert_eq!(grid.prompt_marks_pruned(), 1);

    parser.advance(&mut grid, b"\x1b]133;A\x07");
    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");
    let marks = grid_msgs(&out)
        .into_iter()
        .filter(|m| matches!(m, GridMsg::PromptMark { .. }))
        .count();
    assert_eq!(marks, 1, "the post-prune mark ships once, not skipped");
    let mut out2 = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out2).expect("compose ok");
    assert!(
        grid_msgs(&out2)
            .iter()
            .all(|m| !matches!(m, GridMsg::PromptMark { .. })),
        "no mark re-fires on a quiescent cycle",
    );
}

/// Composition lives on the daemon: the shadow has no scrollback ring, only
/// a mirror of shipped `RowDelta`s.
#[test]
fn compose_diffs_ships_composed_view_on_viewport_flip_into_browse() {
    let mut grid = Grid::new(2, 8);
    let mut parser = felis_vt::Parser::new();
    parser.advance(&mut grid, b"row1\r\nrow2\r\nrow3\r\nrow4\r\nrow5");
    assert!(!grid.scrollback().is_empty());

    let mut stream = seeded_stream(&grid);
    stream.diff.viewport = 1;
    merge_damage(&mut grid, &mut stream);

    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");

    let msgs = grid_msgs(&out);
    let row_entry_count: usize = msgs
        .iter()
        .filter_map(|m| match m {
            GridMsg::RowDelta { rows } => Some(rows.len()),
            _ => None,
        })
        .sum();
    let viewport_state_count = msgs
        .iter()
        .filter(|m| matches!(m, GridMsg::ViewportState { .. }))
        .count();
    let hidden_cursor_count = msgs
        .iter()
        .filter(|m| matches!(m, GridMsg::CursorState { visible: false, .. }))
        .count();
    assert_eq!(
        row_entry_count,
        usize::from(grid.rows()),
        "viewport flip must cover every visible row in the composed RowDelta",
    );
    assert_eq!(
        viewport_state_count, 1,
        "first cycle past drift must emit ViewportState exactly once",
    );
    assert_eq!(
        hidden_cursor_count, 1,
        "cursor must hide on browse entry so the live cursor doesn't render mid-scrollback",
    );
}

/// The continuation bit rides every live-row `RowDelta` so the shadow can
/// stitch logical lines for triple-click selection.
#[test]
fn compose_diffs_carries_the_soft_wrap_bit_on_live_rows() {
    let mut grid = Grid::new(2, 3);
    felis_vt::Parser::new().advance(&mut grid, b"abcdef");
    assert!(grid.row_soft_wrap_continued(1), "fixture must autowrap");

    let mut stream = seeded_stream(&grid);
    merge_damage(&mut grid, &mut stream);
    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");

    let mut bits: Vec<(u16, bool)> = Vec::new();
    for msg in grid_msgs(&out) {
        if let GridMsg::RowDelta { rows } = msg {
            for (row, packed_cells) in rows {
                let decoded =
                    felis_grid::decode_row(&packed_cells.0, &mut felis_grid::StyleTable::new())
                        .unwrap();
                bits.push((row, decoded.soft_wrap_continued));
            }
        }
    }
    bits.sort_unstable();
    assert_eq!(bits, vec![(0, false), (1, true)]);
}

/// The per-cycle row cache is a shared encode: what it hands the second
/// subscriber must be byte-identical to encoding the row again.
#[test]
fn mirrored_subscribers_share_byte_identical_row_encodings() {
    let mut grid = Grid::new(2, 8);
    let mut first = seeded_stream(&grid);
    let mut second = seeded_stream(&grid);
    let mut uncached = seeded_stream(&grid);
    felis_vt::Parser::new().advance(&mut grid, b"hi\r\nthere");
    for stream in [&mut first, &mut second, &mut uncached] {
        stream.damage.merge(grid.damage());
    }
    grid.damage_mut().clear();

    let mut cache = RowEncodeCache::default();
    cache.begin_cycle(2);
    let mut out_first = Vec::new();
    let mut out_second = Vec::new();
    let mut out_uncached = Vec::new();
    compose_diffs(&mut grid, &mut first, &mut cache, &mut out_first).expect("compose ok");
    compose_diffs(&mut grid, &mut second, &mut cache, &mut out_second).expect("compose ok");
    compose_one(&mut grid, &mut uncached, &mut out_uncached).expect("compose ok");

    let row_deltas = |out: &[OutEvent]| -> Vec<(u16, Vec<u8>)> {
        let mut rows: Vec<(u16, Vec<u8>)> = grid_msgs(out)
            .into_iter()
            .filter_map(|msg| match msg {
                GridMsg::RowDelta { rows } => Some(rows),
                _ => None,
            })
            .flatten()
            .map(|(row, packed_cells)| (row, packed_cells.0))
            .collect();
        rows.sort_unstable();
        rows
    };
    let first_rows = row_deltas(&out_first);
    assert_eq!(first_rows.len(), 2, "both dirty rows ship");
    assert_eq!(first_rows, row_deltas(&out_second), "cache hit differs");
    assert_eq!(first_rows, row_deltas(&out_uncached), "cache miss differs");
}

/// Rows above the scrollback seam read the ring's wrapped side-band, rows
/// below it the live grid's bit.
#[test]
fn browse_compose_sources_the_wrap_bit_across_the_scrollback_seam() {
    let mut grid = Grid::new(2, 3);
    let mut parser = felis_vt::Parser::new();
    parser.advance(&mut grid, b"abcdef\r\nxx\r\nyy");
    assert_eq!(grid.scrollback().len(), 2);

    let mut stream = seeded_stream(&grid);
    stream.diff.viewport = 1;
    merge_damage(&mut grid, &mut stream);
    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");

    for msg in grid_msgs(&out) {
        if let GridMsg::RowDelta { rows } = msg {
            for (row, packed_cells) in rows {
                let decoded =
                    felis_grid::decode_row(&packed_cells.0, &mut felis_grid::StyleTable::new())
                        .unwrap();
                assert_eq!(
                    decoded.soft_wrap_continued,
                    row == 0,
                    "visible row {row} ({:?})",
                    row_text(&packed_cells.0),
                );
            }
        }
    }
}

/// Ring rows index negatively (`-1` the youngest), live rows by grid
/// coordinate: the stream `felis sessions capture --source scrollback`
/// prints.
#[test]
fn region_rows_window_scrollback_walks_ring_then_screen() {
    let mut grid = Grid::new(2, 8);
    let mut parser = felis_vt::Parser::new();
    parser.advance(&mut grid, b"row1\r\nrow2\r\nrow3\r\nrow4\r\nrow5");
    assert_eq!(grid.scrollback().len(), 3);

    let rows = region::region_rows_window(
        &grid,
        0,
        felis_protocol::messages::RegionSource::Scrollback,
        false,
        None,
        0,
        usize::MAX,
    )
    .expect("scrollback always resolves");
    let indexed: Vec<(i32, &str)> = rows.iter().map(|r| (r.row, r.text.as_str())).collect();
    assert_eq!(
        indexed,
        vec![
            (-3, "row1"),
            (-2, "row2"),
            (-1, "row3"),
            (0, "row4"),
            (1, "row5")
        ],
    );
    assert!(
        rows.iter().all(|r| r.ansi.is_none()),
        "ansi rides only when the request asks",
    );
}

/// `max_rows` filters without renumbering (the `capture --lines N` wire
/// path).
#[test]
fn region_rows_window_tail_keeps_indices() {
    let mut grid = Grid::new(2, 8);
    let mut parser = felis_vt::Parser::new();
    parser.advance(&mut grid, b"row1\r\nrow2\r\nrow3\r\nrow4\r\nrow5");
    assert_eq!(grid.scrollback().len(), 3);

    let tail = |cap: u32| {
        region::region_rows_window(
            &grid,
            0,
            felis_protocol::messages::RegionSource::Scrollback,
            false,
            Some(cap),
            0,
            usize::MAX,
        )
        .expect("scrollback always resolves")
        .into_iter()
        .map(|r| (r.row, r.text))
        .collect::<Vec<_>>()
    };

    assert_eq!(
        tail(2),
        vec![(0, "row4".to_string()), (1, "row5".to_string())],
    );
    assert_eq!(tail(0), Vec::<(i32, String)>::new());
    assert_eq!(tail(10).len(), 5);
}

/// `None` becomes an empty stream whose terminator still arrives, so the
/// client's collecting loop never hangs.
#[test]
fn region_rows_indexed_unresolved_mark_range_is_none() {
    let grid = Grid::new(24, 80);
    assert!(
        region::region_rows_window(
            &grid,
            0,
            felis_protocol::messages::RegionSource::CommandOutput,
            false,
            None,
            0,
            usize::MAX,
        )
        .is_none(),
    );
}

/// Steady-browse cycles ship no `RowDelta`: kitty / wezterm freeze the
/// visible content while scrolled back, and felis matches.
#[cfg(unix)]
#[test]
fn compose_diffs_freezes_view_during_steady_browse() {
    let mut grid = Grid::new(2, 8);
    let mut parser = felis_vt::Parser::new();
    parser.advance(&mut grid, b"row1\r\nrow2\r\nrow3\r\nrow4\r\nrow5");

    let mut stream = seeded_stream(&grid);
    stream.diff.last_cursor = felis_grid::Cursor {
        row: 0,
        col: 0,
        visible: false,
        pending_wrap: false,
    };
    stream.diff.viewport = 1;
    stream.diff.last_viewport_state = (1u32, viewport_max_for(&grid));
    grid.damage_mut().clear();
    parser.advance(&mut grid, b"more!");
    merge_damage(&mut grid, &mut stream);

    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");

    assert!(
        grid_msgs(&out)
            .iter()
            .all(|m| !matches!(m, GridMsg::RowDelta { .. })),
        "steady browse must not flush PTY-driven RowDeltas — view stays frozen",
    );
    assert!(
        stream.damage.dirty_rows().next().is_some(),
        "damage must remain accumulated for the snap-back flush",
    );
}

/// A subscriber that still owes rows in the band takes the directive
/// too, its owed rows moving with the shift.
#[test]
fn a_subscriber_with_unshipped_rows_takes_the_directive_with_its_rows_moved() {
    let op = felis_grid::ScrollOp {
        region_top: 0,
        region_bottom: 3,
        n_rows: 1,
        direction: felis_grid::ScrollDirection::Up,
    };
    let grid = Grid::new(4, 8);

    let mut clean = seeded_stream(&grid);
    clean.accept_scroll(op);
    assert_eq!(clean.pending_scrolls, vec![op]);
    assert_eq!(clean.damage.dirty_rows().collect::<Vec<_>>(), vec![3]);

    let mut behind = seeded_stream(&grid);
    behind.damage.mark(2);
    behind.accept_scroll(op);
    assert_eq!(behind.pending_scrolls, vec![op]);
    assert_eq!(behind.damage.dirty_rows().collect::<Vec<_>>(), vec![1, 3]);
}

/// Shifts of one band fold into one directive until they would blank it,
/// when the replay restates the band and the directive is dropped.
#[test]
fn shifts_of_one_band_fold_until_the_band_is_owed_whole() {
    let op = |n_rows| felis_grid::ScrollOp {
        region_top: 1,
        region_bottom: 3,
        n_rows,
        direction: felis_grid::ScrollDirection::Up,
    };
    let grid = Grid::new(5, 8);
    let mut stream = seeded_stream(&grid);
    stream.accept_scroll(op(1));
    stream.accept_scroll(op(1));
    assert_eq!(stream.pending_scrolls, vec![op(2)]);
    stream.accept_scroll(op(1));
    assert_eq!(stream.pending_scrolls, Vec::<felis_grid::ScrollOp>::new());
    assert_eq!(
        stream.damage.dirty_rows().collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resize_over_the_wire_makes_subsequent_row_deltas_carry_the_new_width() {
    use felis_grid::decode_row;

    const NEW_COLS: u16 = 120;

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("read x; printf 'got=%s\\n' \"$x\"");

    spawn_daemon(&path, pool, factory).await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;

    hello_welcome(&mut reader, &mut writer, false).await;

    create_and_attach(&mut reader, &mut writer).await;

    send_input(
        &mut writer,
        &InputMsg::Resize {
            dims: felis_protocol::messages::RequestedDims {
                rows: 30,
                cols: u32::from(NEW_COLS),
                pixel_w: 0,
                pixel_h: 0,
            },
        },
    )
    .await;

    send_input(&mut writer, &InputMsg::KeyBytes(b"HELLO\n".to_vec())).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut saw_post_resize_row = false;
    while tokio::time::Instant::now() < deadline {
        let frame = match tokio::time::timeout(Duration::from_secs(5), reader.next_frame()).await {
            Ok(Ok(Some(f))) => f,
            Ok(Err(e)) => panic!("read: {e:?}"),
            Ok(Ok(None)) | Err(_) => break,
        };
        if frame.kind != MessageKind::Grid.as_u16() {
            continue;
        }
        let msg: GridMsg = codec::decode(&frame.body).unwrap();
        if let GridMsg::RowDelta { rows } = msg {
            for (_, packed_cells) in &rows {
                let decoded =
                    decode_row(&packed_cells.0, &mut felis_grid::StyleTable::new()).unwrap();
                let text = row_text(&packed_cells.0);
                if text.contains("got=HELLO") {
                    assert_eq!(
                        decoded.cells.len(),
                        usize::from(NEW_COLS),
                        "post-resize row must be {NEW_COLS} cells wide; \
                         got {} (text: {text:?})",
                        decoded.cells.len(),
                    );
                    saw_post_resize_row = true;
                    break;
                }
            }
            if saw_post_resize_row {
                break;
            }
        }
    }
    assert!(
        saw_post_resize_row,
        "never saw the post-read printf land on a row"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_frame_on_wrong_kind_is_rejected_cleanly() {
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

/// Pins the documented defaults: 5 s post-exit grace (spec REQ-009), 100 ms
/// parked drain.
#[test]
fn idle_policy_default_carries_the_documented_grace_and_drain_interval() {
    let policy = IdlePolicy::default();
    assert_eq!(policy.post_exit_grace, Duration::from_secs(5));
    assert_eq!(policy.drain_interval, Duration::from_millis(100));
    assert_eq!(
        DaemonCaps::default().idle.post_exit_grace,
        policy.post_exit_grace
    );
}

/// Pins the owner task's reap contract (session-lifecycle.md "Same-user
/// mirroring").
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_task_reaps_dead_session_after_grace() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("exit 0");
    let caps = DaemonCaps {
        idle: IdlePolicy {
            post_exit_grace: Duration::from_millis(50),
            drain_interval: Duration::from_millis(20),
        },
        ..DaemonCaps::default()
    };

    spawn_daemon_with_caps(&path, pool.clone(), factory, caps).await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;

    create_and_attach(&mut reader, &mut writer).await;

    drop(reader);
    drop(writer);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(25)).await;
        if pool.lock().await.is_empty() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            let n = pool.lock().await.len();
            panic!("task did not reap dead session within deadline (still {n} live)");
        }
    }
}

/// The session produces no PTY output, so only a `tcgetpgrp` taken while
/// the roster is built can follow the `exec` handover.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_roster_names_the_program_that_owns_the_terminal_now() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    // `exec` keeps the pid (and so the foreground pgid) while replacing the
    // program.
    session_task::spawn_owned(
        &pool,
        owned_session("sleep 3; exec sleep 20"),
        IdlePolicy::default(),
        SessionId::new(),
        0,
        0,
        Vec::new(),
        None,
        Listing::Public,
    )
    .await;

    wait_for_listed_foreground(&pool, "sh").await;
    wait_for_listed_foreground(&pool, "sleep").await;
}

#[cfg(unix)]
async fn wait_for_listed_foreground(pool: &Arc<Mutex<SessionPool>>, expect: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let seen = build_session_list(pool)
            .await
            .first()
            .and_then(|row| row.foreground.clone());
        if seen.as_deref() == Some(expect) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the roster never reported {expect:?} as the foreground program (last: {seen:?})"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// A lingering corpse must list as `exited = true` so a client's automatic
/// pick skips it.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exited_shell_is_flagged_in_meta_during_the_grace() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("exit 0");
    let caps = DaemonCaps {
        idle: IdlePolicy {
            post_exit_grace: Duration::from_secs(60),
            drain_interval: Duration::from_millis(10),
        },
        ..DaemonCaps::default()
    };
    spawn_daemon_with_caps(&path, pool.clone(), factory, caps).await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    let id = create_and_attach(&mut reader, &mut writer).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        // An absent handle is retried rather than failed: publication
        // follows the ack this caller has already read, so a by-name
        // lookup can still miss the row for an instant.
        let handle = pool.lock().await.handle_cloned(SessionId(id));
        if handle.is_some_and(|handle| handle.meta_snapshot().exited) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "meta.exited never flipped after the shell exited"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The selection race end to end: the picked session exits before the
/// attach reaches the daemon, the live-only attach is refused, and the
/// re-pick lands on the survivor.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pick_that_exits_before_the_attach_is_refused_and_the_re_pick_lands() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let caps = DaemonCaps {
        idle: IdlePolicy {
            post_exit_grace: Duration::from_secs(60),
            drain_interval: Duration::from_millis(10),
        },
        ..DaemonCaps::default()
    };
    spawn_daemon_with_caps(&path, pool.clone(), shell_factory("sleep 30"), caps).await;

    let session_task::SessionLifecycle { id: survivor, .. } = session_task::spawn_owned(
        &pool,
        owned_session("sleep 30"),
        IdlePolicy::default(),
        SessionId::new(),
        0,
        0,
        Vec::new(),
        None,
        Listing::Public,
    )
    .await;
    let session_task::SessionLifecycle { id: doomed, .. } = session_task::spawn_owned(
        &pool,
        owned_session("sleep 30"),
        IdlePolicy::default(),
        SessionId::new(),
        0,
        0,
        Vec::new(),
        None,
        Listing::Public,
    )
    .await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Attach {
            target: AttachTarget::Id(survivor.0),
            live_only: true,
        },
    )
    .await;
    assert_eq!(attached_info(&mut reader).await.id, survivor.0);
    drain_rehydrate(&mut reader).await;

    send_request(&mut writer, &OpsToDaemonMsg::List, 1).await;
    let roster = listed_roster(&mut reader).await;
    assert!(
        roster.iter().any(|row| row.id == doomed.0 && !row.exited),
        "the pick's roster must still show the doomed session as live"
    );

    pool.lock()
        .await
        .handle_cloned(doomed)
        .expect("the doomed session is still pooled")
        .cmd
        .send(SessionCmd::Shutdown)
        .await
        .expect("the session task takes the shutdown");
    wait_for_gone(&pool, doomed).await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Attach {
            target: AttachTarget::Id(doomed.0),
            live_only: true,
        },
    )
    .await;
    let refused = reader
        .next_frame()
        .await
        .unwrap()
        .expect("a refusal, not a bare EOF");
    let reason = match codec::decode::<SessionToClientMsg>(&refused.body).unwrap() {
        SessionToClientMsg::AttachFailed { reason, .. } => reason,
        other => panic!("expected AttachFailed, got {other:?}"),
    };
    assert!(
        matches!(
            reason,
            AttachRefusal::Attach(AttachFailure::UnknownSession | AttachFailure::SessionExited)
        ),
        "a vanished target must refuse with a reason that says re-pick, got {reason:?}"
    );

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    send_request(&mut writer, &OpsToDaemonMsg::List, 1).await;
    let roster = listed_roster(&mut reader).await;
    assert!(
        !roster.iter().any(|row| row.id == doomed.0),
        "the refetch must not still offer the session that just went away"
    );
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Attach {
            target: AttachTarget::Id(survivor.0),
            live_only: true,
        },
    )
    .await;
    assert_eq!(attached_info(&mut reader).await.id, survivor.0);
}

#[cfg(unix)]
async fn listed_roster<R>(reader: &mut FrameReader<R>) -> Vec<SessionInfo>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let frame = reader.next_frame().await.unwrap().expect("a Listed reply");
    match codec::decode::<OpsToClientMsg>(&frame.body).unwrap() {
        OpsToClientMsg::Listed { sessions } => sessions,
        other => panic!("expected OpsToClientMsg::Listed, got {other:?}"),
    }
}

#[cfg(unix)]
async fn drain_rehydrate<R>(reader: &mut FrameReader<R>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        let frame = reader
            .next_frame()
            .await
            .unwrap()
            .expect("a rehydrate frame");
        if frame.kind == MessageKind::Grid.as_u16()
            && matches!(
                codec::decode::<GridMsg>(&frame.body).unwrap(),
                GridMsg::RehydrateEnd
            )
        {
            return;
        }
    }
}

#[cfg(unix)]
async fn wait_for_gone(pool: &Arc<Mutex<SessionPool>>, id: SessionId) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if pool.lock().await.handle_cloned(id).is_none() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the session never left the pool"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// `SessionInfo::sequence` orders the switch ring: strictly increasing per
/// creation, stable for a session's life, never reused after a reap.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn creation_sequences_are_monotonic_and_never_reassigned() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let mut created = Vec::new();
    for _ in 0..3 {
        let session_task::SessionLifecycle { id, info, .. } = session_task::spawn_owned(
            &pool,
            owned_session("sleep 30"),
            IdlePolicy::default(),
            SessionId::new(),
            0,
            0,
            Vec::new(),
            None,
            Listing::Public,
        )
        .await;
        created.push((id, info.sequence));
    }
    let sequences: Vec<NonZeroU64> = created.iter().map(|&(_, seq)| seq).collect();
    assert!(
        sequences.windows(2).all(|w| w[0] < w[1]),
        "creation order must run strictly upward, got {sequences:?}"
    );

    let listed = build_session_list(&pool).await;
    for &(id, sequence) in &created {
        let row = listed
            .iter()
            .find(|row| row.id == id.0)
            .expect("every live session is listed");
        assert_eq!(
            row.sequence, sequence,
            "a listing must not renumber a session"
        );
    }

    pool.lock().await.remove(created[0].0);
    let session_task::SessionLifecycle { info: fresh, .. } = session_task::spawn_owned(
        &pool,
        owned_session("sleep 30"),
        IdlePolicy::default(),
        SessionId::new(),
        0,
        0,
        Vec::new(),
        None,
        Listing::Public,
    )
    .await;
    assert!(
        fresh.sequence > sequences[2],
        "a reap leaves a gap; it does not hand its place to the next creation"
    );
}

/// `Created` carries the post-subscribe roster row: the row the spawn
/// minted lists no attachment and would tell the creator its own window
/// is not there.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_ack_reports_the_window_it_attached() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("sleep 30")).await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    let info = create_and_attach_info(&mut reader, &mut writer).await;

    assert_eq!(
        info.attachments.len(),
        1,
        "the create ack's roster row must list the subscriber it answers: {info:?}"
    );
}

/// Stamped at creation and never renumbered: an attach that renumbered it
/// would move the window's anchor mid-switch.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_re_attach_does_not_move_a_session_in_the_ring() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("sleep 30")).await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    let id = create_and_attach_info(&mut reader, &mut writer).await;
    let first = id.sequence;
    let id = id.id;
    drop(reader);
    drop(writer);

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Attach {
            target: AttachTarget::Id(id),
            live_only: false,
        },
    )
    .await;
    assert_eq!(attached_info(&mut reader).await.sequence, first);
}

/// The refusal comes from the session actor, the only place that sees both
/// the exit and the subscribe; the roster is a snapshot and cannot close
/// the window.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_only_attach_is_refused_on_a_corpse_but_a_deliberate_one_lands() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let caps = DaemonCaps {
        idle: IdlePolicy {
            post_exit_grace: Duration::from_secs(60),
            drain_interval: Duration::from_millis(10),
        },
        ..DaemonCaps::default()
    };
    spawn_daemon_with_caps(&path, pool.clone(), shell_factory("exit 0"), caps).await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    let id = create_and_attach(&mut reader, &mut writer).await;
    wait_for_exited(&pool, SessionId(id)).await;
    drop(reader);
    drop(writer);

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Attach {
            target: AttachTarget::Id(id),
            live_only: true,
        },
    )
    .await;
    let refused = reader
        .next_frame()
        .await
        .unwrap()
        .expect("a refusal, not EOF");
    match codec::decode::<SessionToClientMsg>(&refused.body).unwrap() {
        SessionToClientMsg::AttachFailed { reason, .. } => {
            assert_eq!(reason, AttachRefusal::Attach(AttachFailure::SessionExited));
        }
        other => panic!("expected AttachFailed, got {other:?}"),
    }

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Attach {
            target: AttachTarget::Id(id),
            live_only: false,
        },
    )
    .await;
    assert_eq!(attached_info(&mut reader).await.id, id);
}

#[cfg(unix)]
async fn attached_info<R>(reader: &mut FrameReader<R>) -> SessionInfo
where
    R: tokio::io::AsyncRead + Unpin,
{
    let frame = reader.next_frame().await.unwrap().expect("an attach ack");
    match codec::decode::<SessionToClientMsg>(&frame.body).unwrap() {
        SessionToClientMsg::Attached { info } => info,
        other => panic!("expected SessionToClientMsg::Attached, got {other:?}"),
    }
}

#[cfg(unix)]
async fn wait_for_exited(pool: &Arc<Mutex<SessionPool>>, id: SessionId) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        // An absent handle is retried rather than failed: publication
        // follows the ack this caller has already read, so a by-name
        // lookup can still miss the row for an instant.
        let handle = pool.lock().await.handle_cloned(id);
        if handle.is_some_and(|handle| handle.meta_snapshot().exited) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the shell never exited"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// A parked session must keep draining its PTY. The child floods 24 MiB,
/// past `felis-pty`'s bounded reader channel (256 × 64 KiB) plus the kernel
/// PTY buffer; a non-draining daemon wedges it and the pool never empties.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn detached_session_keeps_draining_so_child_does_not_block() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("sleep 0.3; dd if=/dev/zero bs=1048576 count=24 2>/dev/null");
    let caps = DaemonCaps {
        idle: IdlePolicy {
            post_exit_grace: Duration::from_millis(50),
            drain_interval: Duration::from_millis(10),
        },
        ..DaemonCaps::default()
    };

    spawn_daemon_with_caps(&path, pool.clone(), factory, caps).await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;

    let id = create_and_attach(&mut reader, &mut writer).await;

    // The 0.3 s pre-flood sleep keeps the wire quiet, so a short read
    // timeout means fully attached.
    while tokio::time::timeout(Duration::from_millis(100), reader.next_frame())
        .await
        .is_ok_and(|frame| frame.is_ok_and(|f| f.is_some()))
    {}

    drop(reader);
    drop(writer);

    let parked_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let subscribers = pool
            .lock()
            .await
            .get(SessionId(id))
            .map(|h| h.meta_snapshot().subscribers);
        match subscribers {
            Some(0) | None => break,
            Some(_) => assert!(
                tokio::time::Instant::now() < parked_deadline,
                "session never parked after detach"
            ),
        }
    }

    // The parked drain runs at the sink pacer's sustained rate
    // (`crate::parse_sink`, ≈10 MiB/s), so 24 MiB takes a few seconds; the
    // deadline is generous because the parallel suite slows the drain and a
    // tight bound flakes.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        tokio::time::sleep(Duration::from_millis(25)).await;
        if pool.lock().await.is_empty() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "detached child never completed — drain backpressure regressed"
        );
    }
}

/// docs/reference/protocols/kitty-graphics.md: a placement scrolled into
/// history survives with a non-positive anchor row, and the mirror hears
/// the move via `PlacementsShifted`.
#[cfg(unix)]
#[tokio::test]
async fn scroll_into_scrollback_retains_placement_and_emits_shift() {
    use crate::graphics::ImageEvent;
    use felis_grid::images::{CellPos, ImageEntry, ImageFormat, ImageId, Placement, PlacementId};

    let mut session = owned_session("read _x");
    session
        .images
        .insert(
            ImageId(7),
            ImageEntry::new(8, 8, ImageFormat::Rgba32, vec![0xAB; 8 * 8 * 4]),
        )
        .expect("insert under cap");
    session.placements.upsert(Placement {
        image_id: ImageId(7),
        placement_id: Some(PlacementId(1)),
        anchor: CellPos { row: 1, col: 1 },
        cols: 4,
        rows: 4,
        requested_cols: 4,
        requested_rows: 4,
        source: None,
        z_index: 0,
        no_cursor_move: false,
        quiet: 0,
    });

    let rows = usize::from(session.lock_core().grid.rows());
    let buf = vec![b'\n'; rows + 10];
    {
        let mut core = session.lock_core();
        let core = &mut *core;
        core.parser.advance(&mut core.grid, &buf);
    }
    let core = Arc::clone(&session.core);
    drain_effects(&mut session, &mut core.lock()).unwrap();

    let scrolled = u32::try_from(session.lock_core().grid.scrollback().len()).expect("fits");
    assert!(scrolled > 0, "newline flood must reach the scrollback");

    let p = session.placements.iter().next().expect("placement kept");
    let expected_row = 1 - i32::try_from(scrolled).unwrap();
    assert_eq!(p.anchor.row, expected_row);

    let shifted: u32 = session
        .image_events
        .iter()
        .filter_map(|m| match m {
            ImageEvent::PlacementsShifted { lines } => Some(*lines),
            _ => None,
        })
        .sum();
    assert_eq!(shifted, scrolled, "client mirror must hear every shift");
    assert!(
        !session
            .image_events
            .iter()
            .any(|m| matches!(m, ImageEvent::PlacementRemoved { .. })),
        "anchor within retained scrollback must not evict",
    );
}

/// Measurement (ignored): attached end-to-end drain of large input files.
///
/// Subscriber pull-paces at ~60 Hz like the GUI client to measure throughput.
/// Run with `FELIS_BENCH_FILE=/path` and `--run-ignored all --no-capture`.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "measurement; needs FELIS_BENCH_FILE, run with --run-ignored all"]
async fn measure_attached_cat_drain() {
    let file = std::env::var("FELIS_BENCH_FILE")
        .expect("set FELIS_BENCH_FILE to a large ASCII file (see this test's doc comment)");
    let bytes = std::fs::metadata(&file)
        .unwrap_or_else(|e| panic!("stat {file}: {e}"))
        .len();

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory: SessionFactory = {
        let file = file.clone();
        Arc::new(move |_| {
            // `sh -c 'exec cat …'` rather than `/bin/cat`: a store-only
            // host has no `/bin/cat`, and `exec` leaves the same single
            // `cat` on the PTY.
            let mut cmd = Command::new("/bin/sh");
            cmd.args(["-c", &format!("exec cat '{file}'")]);
            cmd.env_clear();
            cmd.env("PATH", fixture_path());
            cmd
        })
    };
    spawn_daemon(&path, pool.clone(), factory).await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, true).await;

    // The payload's capture geometry, not the daemon default: replayed at
    // the wrong grid the stream becomes a wrap+scroll storm. Absent asks
    // for the daemon default.
    let env_axis = |key| {
        std::env::var(key)
            .ok()
            .and_then(|v: String| v.parse::<u32>().ok())
    };
    let dims = env_axis("FELIS_BENCH_ROWS")
        .zip(env_axis("FELIS_BENCH_COLS"))
        .map(|(rows, cols)| felis_protocol::messages::RequestedDims {
            rows,
            cols,
            pixel_w: 0,
            pixel_h: 0,
        });
    let _id = create_and_attach_with_dims(&mut reader, &mut writer, dims).await;
    let start = Instant::now();

    let pull = tokio::spawn(async move {
        loop {
            send_input(&mut writer, &InputMsg::NextGridFrame).await;
            tokio::time::sleep(Duration::from_millis(16)).await;
        }
    });

    let deadline = start + Duration::from_secs(120);
    let mut frames: u64 = 0;
    loop {
        let framed = tokio::time::timeout(Duration::from_secs(10), reader.next_frame())
            .await
            .expect("drain stalled: no frame for 10s");
        match framed {
            Ok(Some(_)) => frames += 1,
            Ok(None) | Err(_) => break,
        }
        assert!(Instant::now() < deadline, "drain never completed");
    }
    let secs = start.elapsed().as_secs_f64();
    pull.abort();
    let mib = bytes as f64 / (1024.0 * 1024.0);
    eprintln!(
        "attached cat drain (pull-paced): {mib:.0} MiB in {secs:.3}s = {:.1} MiB/s \
         ({frames} wire frames)",
        mib / secs,
    );
}

/// Measurement (ignored, timing-dependent): pull-paced cycles a window gets
/// while the child floods visible cells; a `ParseCore` lock that never hands
/// off collapses this to a handful (`docs/explanation/rendering/pipeline.md`).
/// Run with `cargo nextest run --cargo-profile release -p felis-daemon
/// --run-ignored all --no-capture measure_pull_cycles_under_flood`.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "measurement; timing-dependent, run with --run-ignored all"]
async fn measure_pull_cycles_under_flood() {
    use std::fmt::Write as _;

    const WINDOW: Duration = Duration::from_secs(10);

    let tmp = private_dir();
    let frames_file = tmp.path().join("frames");
    let passes_file = tmp.path().join("passes");
    // DOOM-fire's shape: every frame repaints every cell with its own
    // truecolor pair, so the parse thread is never idle waiting on the
    // PTY and each pull finds real damage.
    let mut frames = String::new();
    for frame in 0..64_u32 {
        frames.push_str("\x1b[H");
        for row in 0..24_u32 {
            for col in 0..80_u32 {
                let heat = (frame * 7 + row * 5 + col * 3) % 256;
                write!(
                    frames,
                    "\x1b[38;2;{heat};{};0m\x1b[48;2;0;0;{heat}m\u{2580}",
                    heat / 2
                )
                .unwrap();
            }
            if row < 23 {
                frames.push_str("\r\n");
            }
        }
    }
    std::fs::write(&frames_file, frames).unwrap();

    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory: SessionFactory = {
        let script = format!(
            "i=0; while :; do cat '{f}'; i=$((i+1)); echo $i > '{p}.tmp'; mv '{p}.tmp' '{p}'; done",
            f = frames_file.display(),
            p = passes_file.display(),
        );
        Arc::new(move |_| {
            let mut cmd = Command::new("/bin/sh");
            cmd.args(["-c", &script]);
            cmd.env_clear();
            cmd.env("PATH", fixture_path());
            cmd
        })
    };
    spawn_daemon(&path, pool.clone(), factory).await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, true).await;
    let _id = create_and_attach(&mut reader, &mut writer).await;

    loop {
        let frame = tokio::time::timeout(Duration::from_secs(30), reader.next_frame())
            .await
            .expect("rehydrate stalled")
            .unwrap()
            .expect("frame");
        if frame.kind == MessageKind::Grid.as_u16()
            && matches!(
                codec::decode::<GridMsg>(&frame.body).unwrap(),
                GridMsg::RehydrateEnd
            )
        {
            break;
        }
    }

    let passes = || {
        std::fs::read_to_string(&passes_file)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0)
    };
    let passes_at_start = passes();
    send_input(&mut writer, &InputMsg::NextGridFrame).await;
    let deadline = tokio::time::Instant::now() + WINDOW;
    let mut cycles: u64 = 0;
    while let Ok(framed) = tokio::time::timeout_at(deadline, reader.next_frame()).await {
        let frame = framed.unwrap().expect("daemon closed the stream");
        if frame.kind == MessageKind::Grid.as_u16()
            && matches!(
                codec::decode::<GridMsg>(&frame.body).unwrap(),
                GridMsg::CycleEnd
            )
        {
            cycles += 1;
            send_input(&mut writer, &InputMsg::NextGridFrame).await;
        }
    }
    let passes_at_end = passes();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        passes() > passes_at_end,
        "the child stopped flooding, so the window measured no contention",
    );
    eprintln!(
        "pull cycles under flood: {cycles} in {}s ({} frame-file passes)",
        WINDOW.as_secs(),
        passes_at_end.saturating_sub(passes_at_start),
    );
}

/// Header → Chunks → Complete must arrive for every image, each placement
/// after its image.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rehydrate_replays_persisted_images_and_placements() {
    use felis_grid::images::{CellPos, ImageEntry, ImageFormat, ImageId, Placement, PlacementId};
    use felis_protocol::messages::ImageMsg;

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("read _x");

    spawn_daemon(&path, pool.clone(), factory).await;

    let mut owned = owned_session("read _x");
    owned
        .images
        .insert(
            ImageId(7),
            ImageEntry::new(2, 2, ImageFormat::Rgba32, vec![0xAB; 16]),
        )
        .expect("insert under cap");
    owned.placements.upsert(Placement {
        image_id: ImageId(7),
        placement_id: Some(PlacementId(1)),
        anchor: CellPos { row: 1, col: 1 },
        cols: 2,
        rows: 2,
        requested_cols: 2,
        requested_rows: 2,
        source: None,
        z_index: 0,
        no_cursor_move: false,
        quiet: 0,
    });
    let session_task::SessionLifecycle { id: session_id, .. } = session_task::spawn_owned(
        &pool,
        owned,
        IdlePolicy::default(),
        SessionId::new(),
        0,
        0,
        Vec::new(),
        None,
        Listing::Public,
    )
    .await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Attach {
            target: AttachTarget::Id(session_id.0),
            live_only: false,
        },
    )
    .await;
    let _ack = reader.next_frame().await.unwrap().expect("ack");

    // Cells before images (docs/reference/ipc.md
    // "Cross-host carrier: SSH stdio"):
    // first-byte latency is RTT + cells, not RTT + images.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut images = Vec::new();
    let mut saw_row_delta = false;
    let mut row_delta_before_first_image = None;
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "rehydrate burst did not end within deadline"
        );
        let frame = tokio::time::timeout(Duration::from_secs(2), reader.next_frame())
            .await
            .expect("read timed out")
            .unwrap()
            .expect("frame");
        if frame.kind == MessageKind::Image.as_u16() {
            let msg: ImageMsg = codec::decode(&frame.body).unwrap();
            if row_delta_before_first_image.is_none() && matches!(msg, ImageMsg::Header { .. }) {
                row_delta_before_first_image = Some(saw_row_delta);
            }
            images.push(msg);
        } else if frame.kind == MessageKind::Grid.as_u16() {
            let msg: GridMsg = codec::decode(&frame.body).unwrap();
            if matches!(msg, GridMsg::RowDelta { .. }) {
                saw_row_delta = true;
            }
            if matches!(msg, GridMsg::RehydrateEnd) {
                break;
            }
        }
    }
    assert_eq!(
        row_delta_before_first_image,
        Some(true),
        "cells-first rehydrate order: at least one RowDelta must \
         arrive before any ImageMsg::Header so first-byte latency is \
         dominated by cell encoding, not image encoding",
    );
    assert!(
        images
            .iter()
            .any(|m| matches!(m, ImageMsg::Header { id, .. } if id.0 == 7))
    );
    assert!(
        images
            .iter()
            .any(|m| matches!(m, ImageMsg::Chunk { id, .. } if id.0 == 7))
    );
    assert!(
        images
            .iter()
            .any(|m| matches!(m, ImageMsg::Complete { id, .. } if id.0 == 7))
    );
    let placement_idx = images
        .iter()
        .position(|m| matches!(m, ImageMsg::Placement { image_id, .. } if image_id.0 == 7))
        .expect("placement message");
    let complete_idx = images
        .iter()
        .position(|m| matches!(m, ImageMsg::Complete { id, .. } if id.0 == 7))
        .expect("complete message");
    assert!(
        placement_idx > complete_idx,
        "Placement must follow the image's Complete frame so the client's \
         store has the image before any placement references it",
    );
}

/// Cross-host latency probe (docs/reference/ipc.md
/// "Cross-host carrier: SSH stdio"):
/// byte positions of the first `RowDelta` and first `ImageMsg::Header`,
/// projected to wall-clock under a 100 ms RTT, 10 Mbps model.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn time_to_first_frame_under_simulated_slow_link() {
    use felis_grid::images::{CellPos, ImageEntry, ImageFormat, ImageId, Placement, PlacementId};
    use felis_protocol::messages::ImageMsg;

    // The exact numbers are not load-bearing: the bound holds for any RTT ≥
    // 50 ms and bandwidth ≥ 1 Mbps.
    const RTT_MS: u64 = 100;
    const BANDWIDTH_BPS: u64 = 10_000_000;

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("read _x");

    spawn_daemon(&path, pool.clone(), factory).await;

    // 4 KiB of pixels: without them both landmarks land in one MTU and the
    // gap is framing overhead.
    let mut owned = owned_session("read _x");
    owned
        .images
        .insert(
            ImageId(7),
            ImageEntry::new(32, 32, ImageFormat::Rgba32, vec![0xAB; 32 * 32 * 4]),
        )
        .expect("insert under cap");
    owned.placements.upsert(Placement {
        image_id: ImageId(7),
        placement_id: Some(PlacementId(1)),
        anchor: CellPos { row: 1, col: 1 },
        cols: 4,
        rows: 4,
        requested_cols: 4,
        requested_rows: 4,
        source: None,
        z_index: 0,
        no_cursor_move: false,
        quiet: 0,
    });
    let session_task::SessionLifecycle { id: session_id, .. } = session_task::spawn_owned(
        &pool,
        owned,
        IdlePolicy::default(),
        SessionId::new(),
        0,
        0,
        Vec::new(),
        None,
        Listing::Public,
    )
    .await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Attach {
            target: AttachTarget::Id(session_id.0),
            live_only: false,
        },
    )
    .await;
    let _ack = reader.next_frame().await.unwrap().expect("ack");

    let mut bytes_on_wire: usize = 0;
    let mut bytes_to_first_row_delta: Option<usize> = None;
    let mut bytes_to_first_image_header: Option<usize> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "rehydrate burst did not end within deadline"
        );
        let frame = tokio::time::timeout(Duration::from_secs(2), reader.next_frame())
            .await
            .expect("read timed out")
            .unwrap()
            .expect("frame");
        bytes_on_wire += felis_protocol::frame::HEADER_LEN + frame.body.len();

        if frame.kind == MessageKind::Grid.as_u16() {
            let msg: GridMsg = codec::decode(&frame.body).unwrap();
            if matches!(msg, GridMsg::RowDelta { .. }) && bytes_to_first_row_delta.is_none() {
                bytes_to_first_row_delta = Some(bytes_on_wire);
            }
            if matches!(msg, GridMsg::RehydrateEnd) {
                break;
            }
        } else if frame.kind == MessageKind::Image.as_u16() {
            let msg: ImageMsg = codec::decode(&frame.body).unwrap();
            if matches!(msg, ImageMsg::Header { .. }) && bytes_to_first_image_header.is_none() {
                bytes_to_first_image_header = Some(bytes_on_wire);
            }
        }
    }

    let cells_bytes = bytes_to_first_row_delta.expect("at least one RowDelta");
    let image_bytes = bytes_to_first_image_header.expect("at least one ImageMsg::Header");

    assert!(
        cells_bytes < image_bytes,
        "cells must precede images on the wire (got cells_bytes={cells_bytes}, \
         image_bytes={image_bytes})",
    );

    // One RTT for the handshake + bytes / bandwidth; constant overhead
    // cancels between landmarks.
    let bytes_per_ms = (BANDWIDTH_BPS / 8 / 1000) as f64;
    let ms_to_first_cell = RTT_MS as f64 + cells_bytes as f64 / bytes_per_ms;
    let ms_to_first_image = RTT_MS as f64 + image_bytes as f64 / bytes_per_ms;

    eprintln!(
        "cross-host latency probe (RTT={RTT_MS} ms, BW={BANDWIDTH_BPS} bps):\n  \
         bytes_to_first_RowDelta:    {cells_bytes:>6} → {ms_to_first_cell:>6.2} ms\n  \
         bytes_to_first_ImageHeader: {image_bytes:>6} → {ms_to_first_image:>6.2} ms\n  \
         gap (cells-first benefit):   {:>6} bytes / {:>5.2} ms",
        image_bytes - cells_bytes,
        ms_to_first_image - ms_to_first_cell,
    );

    assert!(
        ms_to_first_cell <= 1.5 * RTT_MS as f64,
        "time-to-first-cell {ms_to_first_cell:.2} ms exceeds 1.5×RTT \
         ({:.2} ms) under the slow-link model",
        1.5 * RTT_MS as f64,
    );
    // The RTT bound admits ~62 KB before the first cell; bound the bytes
    // too.
    assert!(
        cells_bytes <= 16 * 1024,
        "{cells_bytes} bytes on the wire before the first RowDelta — \
         the cells-first preamble should be a few KB",
    );
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

    spawn_daemon(&path, pool.clone(), factory).await;

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

        spawn_daemon(&path, pool.clone(), factory).await;

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

    spawn_daemon(&path, pool.clone(), factory).await;

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

/// Returns `(RowDelta frame count, total dirty rows, Scrolled count)` once
/// `deadline` passes with no new frame.
#[cfg(unix)]
async fn drive_and_count_grid_frames(
    body: &'static str,
    deadline: Duration,
) -> (usize, usize, usize) {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory(body);

    let (client_to_daemon_w, daemon_read) = tokio::io::duplex(256 * 1024);
    let (daemon_write, client_from_daemon_r) = tokio::io::duplex(256 * 1024);

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

    hello_welcome(&mut reader, &mut writer, false).await;

    create_and_attach(&mut reader, &mut writer).await;

    let mut row_delta = 0usize;
    let mut total_dirty_rows = 0usize;
    let mut scrolled = 0usize;
    loop {
        let res = tokio::time::timeout(deadline, reader.next_frame()).await;
        match res {
            Err(_) => break, // deadline elapsed → end of burst
            Ok(Err(_) | Ok(None)) => break,
            Ok(Ok(Some(frame))) => {
                if frame.kind == MessageKind::Grid.as_u16() {
                    let msg: GridMsg = codec::decode(&frame.body).unwrap();
                    match msg {
                        GridMsg::RowDelta { rows } => {
                            row_delta += 1;
                            total_dirty_rows += rows.len();
                        }
                        GridMsg::Scrolled { .. } => {
                            scrolled += 1;
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    drop(writer);
    drop(reader);
    drop(server);

    (row_delta, total_dirty_rows, scrolled)
}

/// docs/reference/ipc.md: each diff cycle collapses its dirty rows into one
/// `RowDelta`.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn row_delta_coalescing_collapses_burst_frames() {
    let body = "printf 'line %s\\n' $(seq 1 120); sleep 0.2";

    let (frames, rows, _scrolled) =
        drive_and_count_grid_frames(body, Duration::from_millis(500)).await;

    assert!(frames > 0, "run must ship some RowDelta; got 0");

    // Strictly fewer frames than rows, not a fixed ratio: PTY chunking and
    // flush coalescing align differently per run.
    assert!(
        frames < rows,
        "{frames} RowDelta frames for {rows} dirty rows — \
         the per-cycle batch collapsed nothing"
    );
}

/// Same-user mirroring (session-lifecycle.md): attach is additive, and
/// input from either fans in to the one PTY.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_subscribers_mirror_one_session() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("read x; printf 'got=%s\\n' \"$x\"; sleep 0.5");

    spawn_daemon(&path, pool.clone(), factory).await;

    let (ra, wa) = connect(&path).await.unwrap();
    let (mut reader_a, mut writer_a) = framed(ra, wa).await;
    hello_welcome(&mut reader_a, &mut writer_a, false).await;
    let id = create_and_attach(&mut reader_a, &mut writer_a).await;

    let (rb, wb) = connect(&path).await.unwrap();
    let (mut reader_b, mut writer_b) = framed(rb, wb).await;
    hello_welcome(&mut reader_b, &mut writer_b, false).await;
    attach_existing(&mut reader_b, &mut writer_b, id).await;

    let subscribers = pool
        .lock()
        .await
        .get(SessionId(id))
        .expect("session pooled")
        .meta_snapshot()
        .subscribers;
    assert_eq!(
        subscribers, 2,
        "attach must be additive (same-user mirroring)"
    );

    send_input(&mut writer_b, &InputMsg::KeyBytes(b"HELLO\n".to_vec())).await;
    expect_row_containing(&mut reader_a, "got=HELLO").await;
    expect_row_containing(&mut reader_b, "got=HELLO").await;
}

/// A late joiner is rehydrated from the current grid while the existing
/// subscriber stays on incremental diffs.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_joiner_rehydrates_output_that_predates_its_attach() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("printf marker-xyz; read _x");

    spawn_daemon(&path, pool, factory).await;

    let (ra, wa) = connect(&path).await.unwrap();
    let (mut reader_a, mut writer_a) = framed(ra, wa).await;
    hello_welcome(&mut reader_a, &mut writer_a, false).await;
    let id = create_and_attach(&mut reader_a, &mut writer_a).await;
    expect_row_containing(&mut reader_a, "marker-xyz").await;

    let (rb, wb) = connect(&path).await.unwrap();
    let (mut reader_b, mut writer_b) = framed(rb, wb).await;
    hello_welcome(&mut reader_b, &mut writer_b, false).await;
    let rows = attach_existing(&mut reader_b, &mut writer_b, id).await;
    assert!(
        rows.iter().any(|r| r.contains("marker-xyz")),
        "late joiner's rehydrate must carry pre-attach output; rows: {rows:?}",
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn detaching_one_mirror_keeps_the_other_streaming() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("read x; printf 'got=%s\\n' \"$x\"; sleep 0.5");

    spawn_daemon(&path, pool.clone(), factory).await;

    let (ra, wa) = connect(&path).await.unwrap();
    let (mut reader_a, mut writer_a) = framed(ra, wa).await;
    hello_welcome(&mut reader_a, &mut writer_a, false).await;
    let id = create_and_attach(&mut reader_a, &mut writer_a).await;

    let (rb, wb) = connect(&path).await.unwrap();
    let (mut reader_b, mut writer_b) = framed(rb, wb).await;
    hello_welcome(&mut reader_b, &mut writer_b, false).await;
    attach_existing(&mut reader_b, &mut writer_b, id).await;

    drop(reader_a);
    drop(writer_a);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let subscribers = pool
            .lock()
            .await
            .get(SessionId(id))
            .expect("session stays pooled")
            .meta_snapshot()
            .subscribers;
        if subscribers == 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "first mirror's detach never registered"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    send_input(&mut writer_b, &InputMsg::KeyBytes(b"HELLO\n".to_vec())).await;
    expect_row_containing(&mut reader_b, "got=HELLO").await;
}

/// The failure unit is the one connection: a refused frame tears down its
/// sender and nothing else.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_corrupt_frame_kills_its_own_connection_and_nothing_else() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("read x; printf 'got=%s\\n' \"$x\"; sleep 0.5");

    spawn_daemon(&path, pool.clone(), factory).await;

    let (ra, wa) = connect(&path).await.unwrap();
    let (mut reader_a, mut writer_a) = framed(ra, wa).await;
    hello_welcome(&mut reader_a, &mut writer_a, false).await;
    let id = create_and_attach(&mut reader_a, &mut writer_a).await;

    let (rb, wb) = connect(&path).await.unwrap();
    let (mut reader_b, mut writer_b) = framed(rb, wb).await;
    hello_welcome(&mut reader_b, &mut writer_b, false).await;
    attach_existing(&mut reader_b, &mut writer_b, id).await;

    writer_a
        .write_frame_unchecked(&Frame {
            kind: 0xBEEF,
            body: b"corrupt",
        })
        .await
        .unwrap();
    writer_a.flush().await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(Ok(None) | Err(_)) =
            tokio::time::timeout(Duration::from_millis(200), reader_a.next_frame()).await
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the corrupt frame never closed its own connection"
        );
    }
    drop(reader_a);
    drop(writer_a);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let subscribers = pool
            .lock()
            .await
            .get(SessionId(id))
            .expect("the session must outlive one peer's corruption")
            .meta_snapshot()
            .subscribers;
        if subscribers == 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the corrupt peer's detach never registered"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    send_input(&mut writer_b, &InputMsg::KeyBytes(b"HELLO\n".to_vec())).await;
    expect_row_containing(&mut reader_b, "got=HELLO").await;
}

/// `ForceDetach` (docs/explanation/architecture/control-surfaces.md) evicts
/// every attached client, keeps the session pooled, and reports
/// `was_attached = true`.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn force_detach_evicts_every_subscriber_and_keeps_the_session() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    // `read` is a `sh` builtin, so the session blocks without a program the
    // host must resolve.
    spawn_daemon(&path, pool.clone(), shell_factory("read _x")).await;

    let (ra, wa) = connect(&path).await.unwrap();
    let (mut reader_a, mut writer_a) = framed(ra, wa).await;
    hello_welcome(&mut reader_a, &mut writer_a, false).await;
    let id = create_and_attach(&mut reader_a, &mut writer_a).await;
    let (rb2, wb2) = connect(&path).await.unwrap();
    let (mut reader_a2, mut writer_a2) = framed(rb2, wb2).await;
    hello_welcome(&mut reader_a2, &mut writer_a2, false).await;
    attach_existing(&mut reader_a2, &mut writer_a2, id).await;

    let (rc, wc) = connect(&path).await.unwrap();
    let (mut reader_c, mut writer_c) = framed(rc, wc).await;
    hello_welcome_as(&mut reader_c, &mut writer_c, ConnectionMode::Ops, false).await;
    send_request(
        &mut writer_c,
        &OpsToDaemonMsg::ForceDetach {
            id_prefix: format!("{}", felis_protocol::SessionHex(id)),
        },
        1,
    )
    .await;
    let reply = reader_c
        .next_frame()
        .await
        .unwrap()
        .expect("session-detached");
    match codec::decode::<OpsToClientMsg>(&reply.body).unwrap() {
        OpsToClientMsg::Detached {
            resolved,
            was_attached,
        } => {
            assert_eq!(resolved, ResolvedId::Ok { id });
            assert!(was_attached, "attached clients report was_attached=true");
        }
        other => panic!("expected Ops::Detached, got {other:?}"),
    }

    for reader in [&mut reader_a, &mut reader_a2] {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut saw_evicted = false;
        while tokio::time::Instant::now() < deadline {
            let Ok(Ok(Some(frame))) =
                tokio::time::timeout(Duration::from_secs(2), reader.next_frame()).await
            else {
                break;
            };
            if frame.kind == MessageKind::Push.as_u16()
                && matches!(
                    codec::decode::<PushMsg>(&frame.body).unwrap(),
                    PushMsg::Evicted { .. }
                )
            {
                saw_evicted = true;
                break;
            }
        }
        assert!(
            saw_evicted,
            "every evicted client must receive PushMsg::Evicted"
        );
    }

    let handle = pool.lock().await.handle_cloned(SessionId(id));
    let meta = handle.expect("session stays pooled").meta_snapshot();
    assert_eq!(meta.subscribers, 0, "all subscribers evicted");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn force_detach_idle_session_reports_not_attached() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("read _x")).await;

    let (rb, wb) = connect(&path).await.unwrap();
    let (mut reader_b, mut writer_b) = framed(rb, wb).await;
    hello_welcome_as(&mut reader_b, &mut writer_b, ConnectionMode::Ops, false).await;
    // Spawned rather than created: a `Create` would attach this
    // connection, and the session would not be idle.
    send_request(
        &mut writer_b,
        &OpsToDaemonMsg::Spawn {
            args: felis_protocol::messages::SpawnArgs::default(),
        },
        1,
    )
    .await;
    let spawned = reader_b.next_frame().await.unwrap().expect("spawned");
    let id = match codec::decode::<OpsToClientMsg>(&spawned.body).unwrap() {
        OpsToClientMsg::Spawned {
            outcome: SpawnOutcome::Ok { info },
        } => info.id,
        other => panic!("expected a successful Spawned, got {other:?}"),
    };

    send_request(
        &mut writer_b,
        &OpsToDaemonMsg::ForceDetach {
            id_prefix: format!("{}", felis_protocol::SessionHex(id)),
        },
        2,
    )
    .await;
    let reply = reader_b
        .next_frame()
        .await
        .unwrap()
        .expect("session-detached");
    match codec::decode::<OpsToClientMsg>(&reply.body).unwrap() {
        OpsToClientMsg::Detached {
            resolved,
            was_attached,
        } => {
            assert_eq!(resolved, ResolvedId::Ok { id });
            assert!(!was_attached, "an idle session reports was_attached=false");
        }
        other => panic!("expected Ops::Detached, got {other:?}"),
    }
}

/// Queue admission is the whole reply: a switch with no window to
/// target answers a typed denial beside `queued: 0`, while a switch
/// whose post-condition already holds answers the same count with no
/// denial at all.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_switch_with_no_targeted_window_separates_the_count_from_the_denial() {
    use felis_protocol::messages::SwitchDenied;

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("read _x")).await;

    let (rb, wb) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(rb, wb).await;
    hello_welcome_as(&mut reader, &mut writer, ConnectionMode::Ops, false).await;

    // Spawned rather than created: a `Create` would attach this
    // connection, and the switch would then have a window to target.
    let mut ids = Vec::new();
    for request_id in [1, 2] {
        send_request(
            &mut writer,
            &OpsToDaemonMsg::Spawn {
                args: felis_protocol::messages::SpawnArgs::default(),
            },
            request_id,
        )
        .await;
        let spawned = reader.next_frame().await.unwrap().expect("spawned");
        match codec::decode::<OpsToClientMsg>(&spawned.body).unwrap() {
            OpsToClientMsg::Spawned {
                outcome: SpawnOutcome::Ok { info },
            } => ids.push(info.id),
            other => panic!("expected a successful Spawned, got {other:?}"),
        }
    }
    let (from, to) = (ids[0], ids[1]);

    let hex = |id| format!("{}", felis_protocol::SessionHex(id));
    send_request(
        &mut writer,
        &OpsToDaemonMsg::Switch {
            from_prefix: hex(from),
            target: SwitchTarget::Session(hex(to)),
            scope: SwitchScope::Default,
        },
        3,
    )
    .await;
    let reply = reader.next_frame().await.unwrap().expect("switched");
    match codec::decode::<OpsToClientMsg>(&reply.body).unwrap() {
        OpsToClientMsg::Switched { queued, denied, .. } => {
            assert_eq!(queued, 0, "nothing took the push");
            assert!(
                matches!(denied, Some(SwitchDenied::NoInputOwner)),
                "a scope that named no window is a denial, got {denied:?}",
            );
        }
        other => panic!("expected Ops::Switched, got {other:?}"),
    }

    send_request(
        &mut writer,
        &OpsToDaemonMsg::Switch {
            from_prefix: hex(from),
            target: SwitchTarget::Session(hex(from)),
            scope: SwitchScope::Default,
        },
        4,
    )
    .await;
    let reply = reader.next_frame().await.unwrap().expect("switched");
    match codec::decode::<OpsToClientMsg>(&reply.body).unwrap() {
        OpsToClientMsg::Switched { queued, denied, .. } => {
            assert_eq!(queued, 0, "a satisfied post-condition pushes nothing");
            assert!(
                denied.is_none(),
                "zero is a count, not a failure, got {denied:?}",
            );
        }
        other => panic!("expected Ops::Switched, got {other:?}"),
    }
}

/// Returns the driver's terminal `Result` rather than reading the wire: the
/// `ConnError` names the mode and the attempt, which a socket harness hides
/// behind a bare EOF.
#[cfg(unix)]
async fn run_gated_control_frame<M>(
    pool: Arc<Mutex<SessionPool>>,
    mode: ConnectionMode,
    gated: M,
    // `None` for an uncorrelated arm: the mode gate under test runs
    // ahead of the envelope either way.
    correlation: Option<Correlation>,
) -> Result<(), ConnError>
where
    M: codec::Correlated + felis_protocol::MinorGated + Directed + Clone + Send + Sync,
{
    let factory = shell_factory("read _x");
    let (client_to_daemon_w, daemon_read) = tokio::io::duplex(64 * 1024);
    let (daemon_write, client_from_daemon_r) = tokio::io::duplex(64 * 1024);

    let server = tokio::spawn(async move {
        handle_stream(
            daemon_read,
            daemon_write,
            DaemonCaps::default(),
            pool,
            factory,
        )
        .await
    });

    let (mut reader, mut writer) = framed(client_from_daemon_r, client_to_daemon_w).await;

    hello_welcome_as(&mut reader, &mut writer, mode, false).await;
    match correlation {
        Some(correlation) => writer.send_correlated(&gated, correlation).await.unwrap(),
        None => writer.send(&gated).await.unwrap(),
    }

    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("handle_stream task should terminate")
        .expect("handle_stream task should not panic")
}

#[cfg(unix)]
async fn create_parked_session(pool: Arc<Mutex<SessionPool>>) -> u128 {
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

    hello_welcome(&mut reader, &mut writer, false).await;
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Create {
            args: felis_protocol::messages::SpawnArgs::default(),
        },
    )
    .await;
    let created = reader.next_frame().await.unwrap().expect("created");
    let id = match codec::decode::<SessionToClientMsg>(&created.body).unwrap() {
        SessionToClientMsg::Created { info } => info.id,
        other => panic!("expected SessionToClientMsg::Created, got {other:?}"),
    };

    drop(writer);
    drop(reader);
    drop(tokio::time::timeout(Duration::from_secs(2), server).await);
    id
}

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
        OpsToDaemonMsg::Destroy {
            id_prefix: format!("{}", felis_protocol::SessionHex(id)),
        },
        Some(Correlation::request(RequestId::new(1).expect("non-zero"))),
    )
    .await;
    assert!(
        matches!(
            result,
            Err(ConnError::ModeDenied {
                mode: ConnectionMode::Window,
                ..
            })
        ),
        "a Destroy on a Window connection must end it, got {result:?}",
    );

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
        assert!(
            matches!(
                result,
                Err(ConnError::ModeDenied {
                    mode: ConnectionMode::Window,
                    ..
                })
            ),
            "a {msg:?} on a Window connection must end it, got {result:?}",
        );
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
        },
    )
    .await;
    assert_eq!(attached_info(&mut reader).await.id, id);
    drain_rehydrate(&mut reader).await;

    let id_prefix = format!("{}", felis_protocol::SessionHex(id));
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
        },
        Some(Correlation::stream(StreamId::new(1).expect("non-zero"))),
    )
    .await;
    assert!(
        matches!(
            result,
            Err(ConnError::ModeDenied {
                mode: ConnectionMode::Window,
                ..
            })
        ),
        "a Subscribe on a Window connection must end it, got {result:?}",
    );
}

/// The attach `Attached` reports the roster row after this subscriber joined:
/// no `idle_seconds` at all, not the parked value.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_ready_reports_the_session_as_no_longer_idle() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("read _x")).await;

    let id = create_parked_session(pool).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Attach {
            target: AttachTarget::Id(id),
            live_only: false,
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
        },
        None,
    )
    .await;
    assert!(
        matches!(
            result,
            Err(ConnError::ModeDenied {
                mode: ConnectionMode::Observer,
                ..
            })
        ),
        "an observer's Attach must end the connection, got {result:?}",
    );
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
        },
        SessionToDaemonMsg::Create {
            args: felis_protocol::messages::SpawnArgs::default(),
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

/// security-model.md "Process and environment boundary": inherit minus the
/// denylist, no allowlist, and `SpawnArgs.env` wins over the identity
/// stamps (REQ-912).
#[test]
fn spawn_args_command_inherits_daemon_env_with_user_overrides_last() {
    let session_id = 0xfe115_u128;
    let args = felis_protocol::messages::SpawnArgs {
        command: "printenv".to_owned(),
        env: vec![("TERM".to_owned(), "user-chosen-term".to_owned())],
        ..Default::default()
    };
    let factory: SessionFactory =
        Arc::new(|_| panic!("a named command must not reach the default-program factory"));
    let cmd = command_from_args(&args, &factory, session_id, None, &ResolvedEnv::birth());
    // Keys are stored folded (uppercase on Windows), so probe keys fold the
    // same way.
    let fold = |k: &OsStr| -> String {
        let s = k.to_string_lossy();
        if cfg!(windows) {
            s.to_ascii_uppercase()
        } else {
            s.into_owned()
        }
    };
    let envs: std::collections::BTreeMap<String, OsString> = cmd
        .get_envs()
        .map(|(k, v)| (fold(k), v.to_os_string()))
        .collect();

    // A test run inside a felis window inherits TERM_PROGRAM=felis, so the
    // stamps are asserted on their own values.
    let stamped = [
        "TERM",
        "COLORTERM",
        "TERM_PROGRAM",
        "TERM_PROGRAM_VERSION",
        "FELIS_SESSION_ID",
        "FELIS_SOCKET",
    ];
    for (key, val) in std::env::vars_os() {
        let name = fold(&key);
        if crate::ENV_DENYLIST.contains(&name.as_str()) {
            assert!(
                !envs.contains_key(&name),
                "denylisted var {name} leaked into the built command",
            );
        } else if !stamped.contains(&name.as_str()) {
            assert_eq!(
                envs.get(&name).map(OsString::as_os_str),
                Some(val.as_os_str()),
                "inherited var {name} did not reach the built command",
            );
        }
    }
    assert_eq!(
        envs.get("TERM").map(OsString::as_os_str),
        Some(OsStr::new("user-chosen-term")),
    );
    let sid = format!("{session_id:032x}");
    assert_eq!(
        envs.get("FELIS_SESSION_ID").map(OsString::as_os_str),
        Some(OsStr::new(sid.as_str())),
    );
    assert!(
        !envs.contains_key("FELIS_SOCKET"),
        "FELIS_SOCKET must not leak from the parent when the daemon has no endpoint",
    );
}

#[cfg(unix)]
#[test]
fn the_shipped_terminfo_leads_a_terminfo_dirs_the_spawn_request_sets() {
    let mut cmd = Command::new("true");
    apply_spawn_posture(
        &mut cmd,
        1,
        None,
        &ResolvedEnv::birth(),
        &[("TERMINFO_DIRS".to_owned(), "/caller/terminfo".to_owned())],
        Some(Path::new("/opt/felis/share/terminfo")),
    );
    assert_eq!(
        cmd.get_envs()
            .find(|(key, _)| *key == "TERMINFO_DIRS")
            .map(|(_, value)| value.to_os_string()),
        Some(OsString::from("/opt/felis/share/terminfo:/caller/terminfo")),
    );
}

/// `FELIS_SESSION_ID` or a denylisted key in `SpawnArgs.env` is refused
/// with the key in the reason (REQ-912).
#[test]
fn spawn_with_args_rejects_a_reserved_env_key() {
    let factory: SessionFactory =
        Arc::new(|_| panic!("the default factory must not run for an invalid SpawnArgs"));
    for key in ["FELIS_SESSION_ID", "VTE_VERSION"] {
        let args = felis_protocol::messages::SpawnArgs {
            env: vec![(key.to_owned(), "x".to_owned())],
            ..Default::default()
        };
        match spawn_with_args(&args, &factory, 0, EnvBaseSource::Birth, None, None) {
            Ok(_) => panic!("reserved env key {key} must be rejected"),
            Err(err) => assert!(
                err.to_string().contains(key) && err.to_string().contains("reserved"),
                "the refusal reason must name the reserved key felis owns, got: {err}"
            ),
        }
    }
}

/// Creation-time tags obey the `OpsToDaemonMsg::Tag` caps.
#[test]
fn spawn_with_args_rejects_an_over_cap_tag_set() {
    let factory: SessionFactory =
        Arc::new(|_| panic!("the default factory must not run for an invalid SpawnArgs"));
    let args = felis_protocol::messages::SpawnArgs {
        tags: (0..=felis_protocol::messages::MAX_SESSION_TAGS)
            .map(|i| format!("t{i}"))
            .collect(),
        ..Default::default()
    };
    match spawn_with_args(&args, &factory, 0, EnvBaseSource::Birth, None, None) {
        Ok(_) => panic!("an over-cap tag set must be rejected"),
        Err(err) => assert!(
            err.to_string().contains("at most"),
            "the refusal reason must name the cap, got: {err}"
        ),
    }
}

/// Args without a command are refused with a reason rather than reaching
/// `Command::new("")` and an opaque ENOENT.
#[test]
fn spawn_with_args_rejects_args_without_a_command() {
    let factory: SessionFactory =
        Arc::new(|_| panic!("the default factory must not run for an invalid SpawnArgs"));
    let args = felis_protocol::messages::SpawnArgs {
        command: String::new(),
        args: vec!["-l".to_owned()],
        ..Default::default()
    };
    // `Session` is not `Debug`, so match rather than `expect_err`.
    match spawn_with_args(&args, &factory, 0, EnvBaseSource::Birth, None, None) {
        Ok(_) => panic!("args without a command must be rejected"),
        Err(err) => assert!(
            err.to_string()
                .contains("SpawnArgs: args without a command"),
            "the refusal reason must name the problem, got: {err}"
        ),
    }
}

/// The fallback chain: a present snapshot over the relay's, the relay's
/// over the daemon's own.
#[test]
fn the_env_base_chain_picks_request_then_relay_then_birth() {
    let request = vec![entry("FROM", "request")];
    let relay: Vec<crate::child_env::EnvEntry> = vec![entry("FROM", "relay")];

    let with_base = felis_protocol::messages::SpawnArgs {
        env_base: Some(request),
        ..Default::default()
    };
    assert!(matches!(
        env_base_source(&with_base, Some(&relay)),
        EnvBaseSource::Request(_)
    ));

    let bare = felis_protocol::messages::SpawnArgs::default();
    assert!(matches!(
        env_base_source(&bare, Some(&relay)),
        EnvBaseSource::Relay(_)
    ));
    assert!(matches!(env_base_source(&bare, None), EnvBaseSource::Birth));

    // Presence, not emptiness: an empty snapshot is still the client's
    // answer.
    let empty = felis_protocol::messages::SpawnArgs {
        env_base: Some(Vec::new()),
        ..Default::default()
    };
    assert!(matches!(
        env_base_source(&empty, Some(&relay)),
        EnvBaseSource::Request(_)
    ));
}

/// On Windows a name is UTF-16LE code units, so `b"SHELL"` is an odd-length
/// string `sanitize_base` refuses; every fixture goes through this.
fn entry(name: &str, value: &str) -> crate::child_env::EnvEntry {
    (
        felis_pty::env_bytes(OsStr::new(name)),
        felis_pty::env_bytes(OsStr::new(value)),
    )
}

fn resolved_request(base: &[crate::child_env::EnvEntry]) -> ResolvedEnv {
    resolve_env_base(
        EnvBaseSource::Request(base),
        None,
        None,
        crate::child_env::TARGET_IS_WINDOWS,
    )
    .expect("a representable base resolves")
}

/// A base replaces the daemon's environment rather than layering over it:
/// an overlay would keep the stale entries the base displaces.
#[test]
fn a_base_replaces_the_daemons_environment() {
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = Command::new("/bin/sh");
        cmd.env("DAEMON_ONLY", "stale");
        cmd
    });
    let args = felis_protocol::messages::SpawnArgs::default();
    let base = vec![entry("FROM_BASE", "fresh")];

    let replaced = command_from_args(&args, &factory, 0, None, &resolved_request(&base));
    assert_eq!(env_of(&replaced, "FROM_BASE").as_deref(), Some("fresh"));
    assert_eq!(
        env_of(&replaced, "DAEMON_ONLY"),
        None,
        "nothing from the daemon's own environment survives a base"
    );

    let birth = command_from_args(&args, &factory, 0, None, &ResolvedEnv::birth());
    assert_eq!(env_of(&birth, "DAEMON_ONLY").as_deref(), Some("stale"));
}

/// Order: base, denylist scrub, identity stamps, explicit `env` last
/// (REQ-912).
#[test]
fn the_order_is_base_then_scrub_then_stamps_then_explicit() {
    let factory: SessionFactory = Arc::new(|_| Command::new("/bin/sh"));
    let args = felis_protocol::messages::SpawnArgs {
        env: vec![("TERM".to_owned(), "user-chosen".to_owned())],
        ..Default::default()
    };
    let base = vec![
        entry("VTE_VERSION", "6003"),
        entry("FELIS_SESSION_ID", "stale"),
        entry("TERM", "from-base"),
        entry("COLORTERM", "from-base"),
        entry("KEEP", "kept"),
    ];

    let cmd = command_from_args(&args, &factory, 0xabc, None, &resolved_request(&base));
    assert_eq!(env_of(&cmd, "KEEP").as_deref(), Some("kept"));
    assert_eq!(
        env_of(&cmd, "VTE_VERSION"),
        None,
        "a denylisted key is scrubbed out of an inherited base, silently"
    );
    assert_eq!(
        env_of(&cmd, "FELIS_SESSION_ID").as_deref(),
        Some(&*format!("{}", felis_protocol::SessionHex(0xabc))),
        "the identity stamp is felis's, never the base's"
    );
    assert_eq!(
        env_of(&cmd, "COLORTERM").as_deref(),
        Some("truecolor"),
        "a stamp overrides the base"
    );
    assert_eq!(
        env_of(&cmd, "TERM").as_deref(),
        Some("user-chosen"),
        "an explicit pair overrides even a stamp (REQ-912)"
    );
}

/// `$SHELL` is part of the environment answer: a warm daemon's names
/// whichever login auto-spawned it.
#[test]
fn the_default_program_takes_the_shell_the_base_resolved() {
    let base = vec![entry("SHELL", "/usr/bin/somesh")];
    assert_eq!(
        SpawnedPty::default_shell_command(Some(&base)).get_program(),
        OsStr::new("/usr/bin/somesh")
    );

    let fallback =
        std::env::var_os("SHELL").unwrap_or_else(|| OsString::from(crate::default_shell()));
    assert_eq!(
        SpawnedPty::default_shell_command(Some(&[])).get_program(),
        fallback
    );
    assert_eq!(
        SpawnedPty::default_shell_command(None).get_program(),
        fallback
    );
}

/// The identity escape hatch is read from the environment the stamp is
/// applied over and never reaches the child (denylisted).
#[test]
fn the_identity_hatch_is_read_from_the_resolved_base() {
    let factory: SessionFactory = Arc::new(|_| Command::new("/bin/sh"));
    let args = felis_protocol::messages::SpawnArgs::default();
    let base = vec![
        entry("FELIS_TERM", "xterm-kitty"),
        entry("FELIS_TERM_PROGRAM", "kitty"),
    ];

    let cmd = command_from_args(&args, &factory, 0, None, &resolved_request(&base));
    assert_eq!(env_of(&cmd, "TERM").as_deref(), Some("xterm-kitty"));
    assert_eq!(env_of(&cmd, "TERM_PROGRAM").as_deref(), Some("kitty"));
    assert_eq!(
        env_of(&cmd, "FELIS_TERM"),
        None,
        "the hatch configures the stamp; it is not part of it"
    );

    if std::env::var_os("FELIS_TERM").is_none() {
        let bare = command_from_args(&args, &factory, 0, None, &resolved_request(&[]));
        assert_eq!(env_of(&bare, "TERM").as_deref(), Some(crate::DEFAULT_TERM));
    }
}

#[cfg(unix)]
fn agent_link_in(dir: &TempDir) -> Arc<crate::agent::AgentLink> {
    Arc::new(
        crate::agent::AgentLink::for_endpoint(&Endpoint::unix(dir.path().join("daemon.sock")))
            .unwrap()
            .unwrap(),
    )
}

/// Only a relayed base has `SSH_AUTH_SOCK` rewritten: a forwarded socket
/// dies with its SSH link, and a child's environment cannot change after
/// exec.
#[cfg(unix)]
#[test]
fn only_a_relay_base_is_repointed_at_the_stable_agent_path() {
    let dir = private_dir();
    let link = agent_link_in(&dir);
    let factory: SessionFactory = Arc::new(|_| Command::new("/bin/sh"));
    let args = felis_protocol::messages::SpawnArgs::default();
    let forwarded = vec![entry("SSH_AUTH_SOCK", "/tmp/ssh-abc/agent.42")];

    let relayed = spawn_command_for_test(
        &args,
        &factory,
        EnvBaseSource::Relay(&forwarded),
        Some(&link),
        None,
    );
    assert_eq!(
        env_of(&relayed, "SSH_AUTH_SOCK").as_deref(),
        link.path().to_str(),
        "a relay base points the child at the daemon's stable link"
    );

    let requested = spawn_command_for_test(
        &args,
        &factory,
        EnvBaseSource::Request(&forwarded),
        Some(&link),
        None,
    );
    assert_eq!(
        env_of(&requested, "SSH_AUTH_SOCK").as_deref(),
        Some("/tmp/ssh-abc/agent.42"),
        "a local dial's own agent path is left alone"
    );
}

/// The link may only name what a child could have inherited: the same
/// sanitization a create runs decides the target.
#[test]
fn the_agent_target_is_derived_from_the_sanitized_block() {
    let forwarded = |env| forwarded_agent_socket(&CarrierBlock { env });

    assert_eq!(
        forwarded(vec![entry("SSH_AUTH_SOCK", "/tmp/ssh-abc/agent.42")]),
        Some(PathBuf::from("/tmp/ssh-abc/agent.42"))
    );

    assert_eq!(
        forwarded(vec![
            entry("BAD=NAME", "x"),
            entry("SSH_AUTH_SOCK", "/tmp/ssh-abc/agent.42"),
        ]),
        None
    );
}

/// A relay that forwarded no agent describes a session that never had one.
#[cfg(unix)]
#[test]
fn a_relay_base_without_an_agent_gains_none() {
    let dir = private_dir();
    let link = agent_link_in(&dir);
    let factory: SessionFactory = Arc::new(|_| Command::new("/bin/sh"));
    let args = felis_protocol::messages::SpawnArgs::default();
    let base = vec![entry("PATH", "/bin")];
    let cmd = spawn_command_for_test(
        &args,
        &factory,
        EnvBaseSource::Relay(&base),
        Some(&link),
        None,
    );
    assert_eq!(env_of(&cmd, "SSH_AUTH_SOCK"), None);
}

fn env_of(cmd: &Command, key: &str) -> Option<String> {
    let wanted = if cfg!(windows) {
        key.to_ascii_uppercase()
    } else {
        key.to_owned()
    };
    cmd.get_envs()
        .find(|(k, _)| *k == OsStr::new(&wanted))
        .map(|(_, v)| v.to_string_lossy().into_owned())
}

#[cfg(unix)]
fn spawn_command_for_test(
    args: &felis_protocol::messages::SpawnArgs,
    factory: &SessionFactory,
    base: EnvBaseSource<'_>,
    agent: Option<&Arc<crate::agent::AgentLink>>,
    endpoint: Option<&OsStr>,
) -> Command {
    let resolved = resolve_env_base(
        base,
        agent.map(AsRef::as_ref),
        endpoint,
        crate::child_env::TARGET_IS_WINDOWS,
    )
    .unwrap();
    command_from_args(args, factory, 0, None, &resolved)
}

/// The default-program path still carries `SpawnArgs.cwd`; an empty one
/// means inherit the daemon's cwd.
#[test]
fn command_from_args_applies_the_requested_cwd_to_the_default_program() {
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = Command::new("/bin/sh");
        cmd.env_clear();
        cmd
    });

    let requested = felis_protocol::messages::SpawnArgs {
        cwd: "/tmp".to_owned(),
        ..Default::default()
    };
    let with_cwd = command_from_args(
        &requested,
        &factory,
        0,
        spawn_cwd(&requested),
        &ResolvedEnv::birth(),
    );
    assert_eq!(with_cwd.get_cwd(), Some(Path::new("/tmp")));

    let bare = felis_protocol::messages::SpawnArgs::default();
    let without = command_from_args(&bare, &factory, 0, spawn_cwd(&bare), &ResolvedEnv::birth());
    assert_eq!(without.get_cwd(), None);
}

/// A relative `cwd` is refused rather than resolved against the daemon's
/// frozen directory.
#[test]
fn spawn_with_args_refuses_a_relative_cwd() {
    let factory: SessionFactory =
        Arc::new(|_| panic!("the default factory must not run for an invalid SpawnArgs"));
    let args = felis_protocol::messages::SpawnArgs {
        cwd: "build".to_owned(),
        ..Default::default()
    };
    match spawn_with_args(&args, &factory, 0, EnvBaseSource::Birth, None, None) {
        Ok(_) => panic!("a relative cwd must be rejected"),
        Err(err) => assert!(
            err.to_string().contains("cwd must be absolute"),
            "the refusal reason must name the problem, got: {err}"
        ),
    }
}

/// An unusable cwd retries in the daemon's own for a bare window launch; a
/// named command keeps failing, since the directory is part of what was
/// asked.
#[cfg(unix)]
#[test]
fn spawn_with_args_falls_back_to_the_daemon_cwd_only_for_a_bare_launch() {
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "exit 0"]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    });
    let missing = "/felis-no-such-directory-for-tests";

    let bare = felis_protocol::messages::SpawnArgs {
        cwd: missing.to_owned(),
        ..Default::default()
    };
    // `Session` is not `Debug`, so match rather than `expect`.
    if let Err(err) = spawn_with_args(&bare, &factory, 0, EnvBaseSource::Birth, None, None) {
        panic!("a bare launch must survive an unusable cwd, got: {err}");
    }

    let named = felis_protocol::messages::SpawnArgs {
        command: "/bin/sh".to_owned(),
        args: vec!["-c".to_owned(), "exit 0".to_owned()],
        cwd: missing.to_owned(),
        ..Default::default()
    };
    assert!(
        spawn_with_args(&named, &factory, 0, EnvBaseSource::Birth, None, None).is_err(),
        "a named command must not be run somewhere other than its requested cwd",
    );
}

/// Each `SpawnArgs` field falls back independently (docs/reference/ipc.md),
/// so `env` reaches the default program too.
#[test]
fn command_from_args_applies_env_overrides_to_the_default_program() {
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = Command::new("/bin/sh");
        cmd.env_clear();
        cmd
    });

    let args = felis_protocol::messages::SpawnArgs {
        env: vec![("FELIS_ENV_PROBE".to_owned(), "reached".to_owned())],
        ..Default::default()
    };
    let cmd = command_from_args(&args, &factory, 0, None, &ResolvedEnv::birth());
    let probe = cmd
        .get_envs()
        .find(|(k, _)| *k == OsStr::new("FELIS_ENV_PROBE"))
        .map(|(_, v)| v.to_owned());
    assert_eq!(probe, Some(OsString::from("reached")));
}

/// Out-of-bound geometry on a create is refused with the dedicated reason
/// before anything is execed (REQ-605a); the factory panics if it runs.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_create_past_the_geometry_bound_is_refused_before_anything_spawns() {
    for rows in [65_536_u32, u32::MAX] {
        let tmp = private_dir();
        let path = tmp.path().join("daemon.sock");
        let pool = Arc::new(Mutex::new(SessionPool::new()));
        let factory: SessionFactory =
            Arc::new(|_| panic!("an out-of-range create must not reach the factory"));
        spawn_daemon(&path, pool, factory).await;

        let (read_half, write_half) = connect(&path).await.unwrap();
        let (mut reader, mut writer) = framed(read_half, write_half).await;
        hello_welcome(&mut reader, &mut writer, false).await;

        send_kind(
            &mut writer,
            &SessionToDaemonMsg::Create {
                args: felis_protocol::messages::SpawnArgs {
                    dims: Some(felis_protocol::messages::RequestedDims {
                        rows,
                        cols: 80,
                        pixel_w: 0,
                        pixel_h: 0,
                    }),
                    ..Default::default()
                },
            },
        )
        .await;

        let refusal = reader.next_frame().await.unwrap().expect("attach-failed");
        match codec::decode::<SessionToClientMsg>(&refusal.body).unwrap() {
            SessionToClientMsg::AttachFailed { reason, detail } => {
                assert_eq!(
                    reason,
                    AttachRefusal::Create(CreateFailure::GeometryOutOfRange)
                );
                assert!(
                    detail.contains("rows") && detail.contains(&rows.to_string()),
                    "the refusal must name the axis and the value it saw, got: {detail}"
                );
            }
            other => panic!("expected AttachFailed, got {other:?}"),
        }
    }
}

/// Absence is the only way a create asks for the daemon default: an
/// omitted geometry lands on 24x80, and a create that spells a zero
/// axis out is refused rather than defaulted (REQ-605a).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_absent_create_geometry_defaults_and_a_zero_axis_is_refused() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool, shell_factory("read x")).await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    let info = create_and_attach_info(&mut reader, &mut writer).await;
    assert_eq!(
        (info.dims.rows, info.dims.cols),
        (crate::DEFAULT_ROWS, crate::DEFAULT_COLS),
    );

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Create {
            args: felis_protocol::messages::SpawnArgs {
                dims: Some(felis_protocol::messages::RequestedDims {
                    rows: 0,
                    cols: 80,
                    pixel_w: 0,
                    pixel_h: 0,
                }),
                ..Default::default()
            },
        },
    )
    .await;
    let refusal = reader.next_frame().await.unwrap().expect("attach-failed");
    match codec::decode::<SessionToClientMsg>(&refusal.body).unwrap() {
        SessionToClientMsg::AttachFailed { reason, detail } => {
            assert_eq!(
                reason,
                AttachRefusal::Create(CreateFailure::GeometryOutOfRange)
            );
            assert!(
                detail.contains("rows 0"),
                "the refusal must name the zero axis, got: {detail}"
            );
        }
        other => panic!("expected AttachFailed, got {other:?}"),
    }
}

/// The same values on a resize are clamped instead: a resize the user did
/// not ask for must never cost them their shell (REQ-605a).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resize_past_the_geometry_bound_is_clamped_and_reported_clamped() {
    use felis_protocol::messages::{MAX_GRID_COLS, MAX_GRID_ROWS};

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool, shell_factory("read x")).await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    create_and_attach(&mut reader, &mut writer).await;

    send_input(
        &mut writer,
        &InputMsg::Resize {
            dims: felis_protocol::messages::RequestedDims {
                rows: u32::MAX,
                cols: 65_536,
                pixel_w: u32::MAX,
                pixel_h: 0,
            },
        },
    )
    .await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut announced = None;
    while announced.is_none() && tokio::time::Instant::now() < deadline {
        let frame = match tokio::time::timeout(Duration::from_secs(5), reader.next_frame()).await {
            Ok(Ok(Some(f))) => f,
            Ok(Err(e)) => panic!("the connection must survive an out-of-range resize: {e:?}"),
            Ok(Ok(None)) | Err(_) => panic!("the daemon closed instead of clamping"),
        };
        if frame.kind != MessageKind::Grid.as_u16() {
            continue;
        }
        if let GridMsg::Size { dims } = codec::decode::<GridMsg>(&frame.body).unwrap() {
            announced = Some(dims);
        }
    }
    let dims = announced.expect("the daemon must announce the clamped geometry");
    assert_eq!(dims.rows, MAX_GRID_ROWS);
    assert_eq!(dims.cols, MAX_GRID_COLS);
    assert_eq!(dims.pixel_w, felis_protocol::messages::MAX_GRID_PIXELS);
    // The unknown pixel sentinel is not a value to clamp.
    assert_eq!(dims.pixel_h, 0);
}

/// The cap refuses the create typed, with nothing executed, and the
/// connection survives.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_past_the_session_cap_is_refused_and_executes_nothing() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let caps = DaemonCaps {
        max_sessions: 1,
        ..DaemonCaps::default()
    };
    spawn_daemon_with_caps(&path, pool.clone(), shell_factory("sleep 30"), caps).await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    create_and_attach(&mut reader, &mut writer).await;

    // A second connection, because the first is attached now: a `Create`
    // is a pre-attach frame.
    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader_b, mut writer_b) = framed(read_half, write_half).await;
    hello_welcome(&mut reader_b, &mut writer_b, false).await;
    send_kind(
        &mut writer_b,
        &SessionToDaemonMsg::Create {
            args: felis_protocol::messages::SpawnArgs::default(),
        },
    )
    .await;
    let refused = reader_b.next_frame().await.unwrap().expect("a refusal");
    match codec::decode::<SessionToClientMsg>(&refused.body).unwrap() {
        SessionToClientMsg::AttachFailed { reason, detail } => {
            assert_eq!(
                reason,
                AttachRefusal::Create(CreateFailure::SessionLimitReached)
            );
            assert!(detail.contains("at 1 of 1 sessions"), "{detail}");
        }
        other => panic!("expected AttachFailed, got {other:?}"),
    }

    // The cap is charged the same way through the headless entry point.
    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader_c, mut writer_c) = framed(read_half, write_half).await;
    hello_welcome_as(&mut reader_c, &mut writer_c, ConnectionMode::Ops, false).await;
    send_request(
        &mut writer_c,
        &OpsToDaemonMsg::Spawn {
            args: felis_protocol::messages::SpawnArgs::default(),
        },
        1,
    )
    .await;
    let refused = reader_c.next_frame().await.unwrap().expect("a refusal");
    match codec::decode::<OpsToClientMsg>(&refused.body).unwrap() {
        OpsToClientMsg::Spawned {
            outcome: SpawnOutcome::Refused { reason, detail },
        } => {
            assert_eq!(reason, CreateFailure::SessionLimitReached);
            assert!(detail.contains("at 1 of 1 sessions"), "{detail}");
        }
        other => panic!("expected a refused Spawned, got {other:?}"),
    }

    assert_eq!(
        pool.lock().await.len(),
        1,
        "the refused creates must leave the pool where it was"
    );
}

/// Two spawns in flight on one connection: each reply carries its own
/// `request_id` and its own session. This daemon serves a connection's
/// requests in arrival order, so the reordered case is pinned against a
/// controlled peer in `felis-cli`'s bridge suite instead.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_ops_spawns_are_attributed_by_request_id() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("sleep 30")).await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome_as(&mut reader, &mut writer, ConnectionMode::Ops, false).await;

    // Tagged so a reply traces back to its request without depending on
    // the session id the daemon minted.
    for (request, tag) in [(1_u64, "first"), (2, "second")] {
        send_request(
            &mut writer,
            &OpsToDaemonMsg::Spawn {
                args: felis_protocol::messages::SpawnArgs {
                    tags: vec![tag.to_owned()],
                    ..Default::default()
                },
            },
            request,
        )
        .await;
    }

    let mut seen = Vec::new();
    for _ in 0..2 {
        let frame = reader.next_frame().await.unwrap().expect("a spawn reply");
        let request = codec::peek_correlation(&frame.body)
            .unwrap()
            .and_then(Correlation::request_id)
            .expect("a spawn reply carries its request id");
        match codec::decode::<OpsToClientMsg>(&frame.body).unwrap() {
            OpsToClientMsg::Spawned {
                outcome: SpawnOutcome::Ok { info },
            } => seen.push((request.get(), info.tags, info.id)),
            other => panic!("expected a successful Spawned, got {other:?}"),
        }
    }
    seen.sort_by_key(|(request, _, _)| *request);
    assert_eq!(seen[0].1, vec!["first".to_owned()]);
    assert_eq!(seen[1].1, vec!["second".to_owned()]);
    assert_ne!(seen[0].2, seen[1].2, "two spawns are two sessions");
    assert_eq!(pool.lock().await.len(), 2);
}

/// The create ack means "attached" and "nameable": the rehydrate burst
/// follows it with no `Attach` in between, and any connection can
/// already look the session up by id.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_is_attached_when_its_ack_is_written() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(
        &path,
        pool.clone(),
        shell_factory("printf 'hello-felis\n'; sleep 30"),
    )
    .await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    let id = create_and_attach(&mut reader, &mut writer).await;

    let subscribers = pool
        .lock()
        .await
        .get(SessionId(id))
        .expect("an acked session must already be nameable")
        .meta_snapshot()
        .subscribers;
    assert_eq!(subscribers, 1, "the create ack must mean attached");
    expect_row_containing(&mut reader, "hello-felis").await;
}

/// Until a create publishes its row, no other connection can list,
/// resolve, attach to or destroy the session it may still roll back.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_in_flight_is_unnameable_until_it_publishes() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "exec sleep 30"]);
        cmd.env_clear();
        cmd.env("PATH", fixture_path());
        cmd
    });
    let caps = DaemonCaps::default();
    let (registered, info) = create_session(
        CreateCtx {
            pool: &pool,
            caps: &caps,
            factory: &factory,
            relay_env: None,
        },
        felis_protocol::messages::SpawnArgs::default(),
    )
    .await
    .expect("the create must be admitted");
    let id = SessionId(info.id);

    {
        let guard = pool.lock().await;
        assert!(guard.handle_cloned(id).is_none(), "attachable too early");
        assert!(guard.roster_by_recency().is_empty(), "listable too early");
        assert_eq!(guard.ids().count(), 0, "resolvable too early");
        assert_eq!(guard.len(), 1, "but it holds a session slot");
    }

    registered.publish().await;
    drop(registered.keep());
    let guard = pool.lock().await;
    assert!(guard.handle_cloned(id).is_some());
    assert_eq!(guard.roster_by_recency().len(), 1);
}

/// A create whose subscribe cannot land leaves nothing in the pool, and
/// its rollback does not return until the child is reaped.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rolled_back_create_leaves_no_pool_entry_and_no_child() {
    let tmp = private_dir();
    let pid_file = tmp.path().join("child.pid");
    let body = format!("printf '%s' $$ > {}; exec sleep 30", pid_file.display());
    rolls_back_a_create_and_reaps(body, &pid_file).await;
}

/// Rollback reaps a child that ignores hangup via `SIGKILL` within timeout.
///
/// `trap '' HUP` survives `exec` so `SIGKILL` cleanup is exercised directly.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rolled_back_create_reaps_a_child_that_ignores_the_hangup() {
    let tmp = private_dir();
    let pid_file = tmp.path().join("child.pid");
    let body = format!(
        "trap '' HUP; printf '%s' $$ > {}; exec sleep 30",
        pid_file.display()
    );
    rolls_back_a_create_and_reaps(body, &pid_file).await;
}

#[cfg(unix)]
async fn rolls_back_a_create_and_reaps(body: String, pid_file: &Path) {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory: SessionFactory = Arc::new(move |_| {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", &body]);
        cmd.env_clear();
        cmd.env("PATH", fixture_path());
        cmd
    });
    let caps = DaemonCaps::default();
    let (mut registered, _info) = create_session(
        CreateCtx {
            pool: &pool,
            caps: &caps,
            factory: &factory,
            relay_env: None,
        },
        felis_protocol::messages::SpawnArgs::default(),
    )
    .await
    .expect("the create must be admitted");
    assert_eq!(pool.lock().await.len(), 1);

    let pid = read_pid(pid_file).await;
    let mut done = registered.done.clone();
    registered.roll_back().await;

    assert_eq!(
        pool.lock().await.len(),
        0,
        "the rolled-back session must leave the pool"
    );
    assert!(
        *done.borrow_and_update(),
        "roll_back must not return before the owner task finished reaping"
    );
    assert!(!pid_alive(pid), "the child must be gone, not orphaned");
}

/// A peer holding the ack can hand its id to another connection at once,
/// so the row must answer lookups before the ack can be read. The
/// carrier here is a byte or two wide, so the ack cannot complete until
/// this test reads it.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_is_nameable_before_its_ack_is_read() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));

    let (client_to_daemon_w, daemon_read) = tokio::io::duplex(64 * 1024);
    // Narrow on purpose: every daemon write blocks until this test
    // reads it, which is what makes the ordering observable.
    let (daemon_write, client_from_daemon_r) = tokio::io::duplex(8);
    let server_pool = pool.clone();
    let server = tokio::spawn(async move {
        handle_stream(
            daemon_read,
            daemon_write,
            DaemonCaps::default(),
            server_pool,
            shell_factory("read _x"),
        )
        .await
    });

    let (mut reader, mut writer) = framed(client_from_daemon_r, client_to_daemon_w).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Create {
            args: felis_protocol::messages::SpawnArgs::default(),
        },
    )
    .await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let id = loop {
        let listed = pool.lock().await.ids().next();
        if let Some(id) = listed {
            break id;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the create never became nameable while its ack was unread"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    let ack = reader.next_frame().await.unwrap().expect("created");
    match codec::decode::<SessionToClientMsg>(&ack.body).unwrap() {
        SessionToClientMsg::Created { info } => assert_eq!(SessionId(info.id), id),
        other => panic!("expected SessionToClientMsg::Created, got {other:?}"),
    }

    drop(writer);
    drop(reader);
    // A hangup that lands mid-rehydrate may end the connection with a
    // broken pipe, so only its ending is asserted.
    let served = tokio::time::timeout(Duration::from_secs(10), server).await;
    shut_down_and_reap(&pool, id).await;
    let _hangup = served
        .expect("the connection must end once its peer hangs up")
        .expect("handle_stream must not panic");
}

/// A creation still in flight appears in the status row: reading `0`
/// while a concurrent create is refused at the cap would name a limit
/// the refusal disagrees with (REQ-915).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_status_counts_a_creation_still_in_flight() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("read _x");
    let caps = DaemonCaps::default();
    let (registered, _info) = create_session(
        CreateCtx {
            pool: &pool,
            caps: &caps,
            factory: &factory,
            relay_env: None,
        },
        felis_protocol::messages::SpawnArgs::default(),
    )
    .await
    .expect("the create must be admitted");
    assert_eq!(
        pool.lock().await.ids().count(),
        0,
        "held, so not yet listed"
    );

    let reply = daemon_status(&pool, &caps).await;
    let OpsToClientMsg::StatusReply { resources, .. } = reply else {
        panic!("expected an OpsToClientMsg::StatusReply, got {reply:?}")
    };
    let sessions = resources
        .iter()
        .find(|row| row.resource == felis_protocol::messages::ResourceKind::Sessions)
        .expect("the reply must carry the sessions row");
    assert_eq!(
        sessions.total_used, 1,
        "a held creation is admitted against the cap and must be counted"
    );

    drop(registered.keep());
}

/// A create is published before its ack, so an ack that never lands
/// leaves the session detached and listed, as a lost `Spawned` does.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_whose_ack_cannot_be_written_leaves_a_detached_session() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));

    let (client_to_daemon_w, daemon_read) = tokio::io::duplex(64 * 1024);
    let (daemon_write, client_from_daemon_r) = tokio::io::duplex(64 * 1024);
    let server_pool = pool.clone();
    let server = tokio::spawn(async move {
        handle_stream(
            daemon_read,
            daemon_write,
            DaemonCaps::default(),
            server_pool,
            shell_factory("read _x"),
        )
        .await
    });

    let (mut reader, mut writer) = framed(client_from_daemon_r, client_to_daemon_w).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    // The peer goes away between its `Create` and the ack, so every
    // later daemon write is a broken pipe.
    drop(reader);
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Create {
            args: felis_protocol::messages::SpawnArgs::default(),
        },
    )
    .await;

    let served = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("the connection must end once its ack cannot be written")
        .expect("handle_stream must not panic");
    assert!(served.is_err(), "the failed ack write ends the connection");
    let listed = pool.lock().await.ids().next();
    let id = listed.expect("an undeliverable ack must leave the session listed");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let subscribers = pool
            .lock()
            .await
            .get(id)
            .expect("still listed")
            .meta_snapshot()
            .subscribers;
        if subscribers == 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the connection whose ack failed must not stay subscribed"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    shut_down_and_reap(&pool, id).await;
}

/// Returns once the session task has left the pool and its child is
/// reaped: `quiescent` stays false until the teardown guard drops.
#[cfg(unix)]
async fn shut_down_and_reap(pool: &Arc<Mutex<SessionPool>>, id: SessionId) {
    let handle = pool.lock().await.handle_cloned(id).expect("listed");
    handle
        .cmd
        .send(SessionCmd::Shutdown)
        .await
        .expect("the session task is still running");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !pool.lock().await.quiescent() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the session was never reaped"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The shell writes its pid before `exec`ing the long sleep, so the
/// value is the process the daemon must reap.
#[cfg(unix)]
async fn read_pid(path: &Path) -> i32 {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(raw) = std::fs::read_to_string(path)
            && let Ok(pid) = raw.trim().parse()
        {
            return pid;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the fixture child never reported its pid"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// `kill -0` from the ambient `PATH`, not [`fixture_path`]: the
/// fixtures are stand-ins for a session's *child*, and this probe needs
/// the real utility. A zombie answers `kill -0`, so an unreaped exit
/// still reads as alive.
#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("`kill -0` must be runnable")
        .success()
}

/// Registration happens after the PTY fork/exec, so a cap that merely reads
/// the pool count admits every create that reached the check before the
/// first one registered; the slot must not overshoot either way.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_creates_never_admit_past_the_session_cap() {
    const CAP: usize = 3;
    const CREATES: usize = 12;

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let caps = DaemonCaps {
        max_sessions: CAP,
        ..DaemonCaps::default()
    };
    spawn_daemon_with_caps(&path, pool.clone(), shell_factory("sleep 30"), caps).await;

    // All dialed before any sends, so the creates reach the admission check
    // together.
    let mut clients = Vec::new();
    for _ in 0..CREATES {
        let (read_half, write_half) = connect(&path).await.unwrap();
        let (reader, writer) = framed(read_half, write_half).await;
        clients.push((reader, writer));
    }
    let mut running = Vec::new();
    for (mut reader, mut writer) in clients {
        running.push(tokio::spawn(async move {
            hello_welcome(&mut reader, &mut writer, false).await;
            send_kind(
                &mut writer,
                &SessionToDaemonMsg::Create {
                    args: felis_protocol::messages::SpawnArgs::default(),
                },
            )
            .await;
            let frame = reader.next_frame().await.unwrap().expect("an answer");
            codec::decode::<SessionToClientMsg>(&frame.body).unwrap()
        }));
    }

    let mut created = 0;
    let mut refused = 0;
    for task in running {
        match task.await.unwrap() {
            SessionToClientMsg::Created { .. } => created += 1,
            SessionToClientMsg::AttachFailed { reason, detail } => {
                assert_eq!(
                    reason,
                    AttachRefusal::Create(CreateFailure::SessionLimitReached)
                );
                assert!(detail.contains(&format!("of {CAP} sessions")), "{detail}");
                refused += 1;
            }
            other => panic!("expected Ready or AttachFailed, got {other:?}"),
        }
    }

    assert_eq!(created, CAP, "the burst must fill the cap exactly");
    assert_eq!(refused, CREATES - CAP);
    assert_eq!(
        pool.lock().await.len(),
        CAP,
        "the pool must never hold more sessions than the cap admits"
    );
}

/// `Ops::Status` carries a row for every accounted resource; a missing row
/// is the bug.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_reports_every_accounted_resource_with_its_limit() {
    use felis_protocol::messages::{ResourceKind, ResourceUnit, SubjectKind};

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let caps = DaemonCaps {
        max_sessions: 7,
        ..DaemonCaps::default()
    };
    spawn_daemon_with_caps(&path, pool.clone(), shell_factory("sleep 30"), caps).await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    let _id = create_and_attach(&mut reader, &mut writer).await;
    drain_rehydrate(&mut reader).await;

    send_request(&mut writer, &OpsToDaemonMsg::Status, 1).await;
    let reply = loop {
        let frame = reader.next_frame().await.unwrap().expect("a Status reply");
        if frame.kind != MessageKind::Ops.as_u16() {
            continue;
        }
        break codec::decode::<OpsToClientMsg>(&frame.body).unwrap();
    };
    let OpsToClientMsg::StatusReply {
        resources,
        worker_threads,
        draining,
    } = reply
    else {
        panic!("expected OpsToClientMsg::StatusReply, got {reply:?}");
    };
    assert!(!draining, "a daemon nobody stopped is not draining");

    let find = |kind: ResourceKind| {
        resources
            .iter()
            .find(|r| r.resource == kind)
            .unwrap_or_else(|| panic!("no {kind:?} row in {resources:?}"))
    };
    assert!(
        worker_threads > 0,
        "the runtime this daemon runs on has workers"
    );
    for kind in [
        ResourceKind::Connections,
        ResourceKind::Sessions,
        ResourceKind::ImageStoreBytes,
        ResourceKind::InFlightDecodes,
        ResourceKind::InFlightDecodeBytes,
        ResourceKind::SubscriberQueueBytes,
    ] {
        let row = find(kind);
        assert!(
            per_subject_limit(row)
                .or_else(|| global_limit(row))
                .is_some(),
            "{kind:?} must report the ceiling it is admitted against"
        );
        // The scope arm is the whole point of the shape: a subject
        // sample belongs to a row that has subjects, and the type
        // already keeps a per-subject ceiling off a daemon row.
        assert_eq!(
            max_subject_used(row).is_some(),
            subject_kind(row).is_some(),
            "{kind:?} carries a deepest subject exactly when it has subjects: {row:?}"
        );
    }
    let connections = find(ResourceKind::Connections);
    assert_eq!(
        connections.total_used, 1,
        "this query is the only connection"
    );
    assert_eq!(
        global_limit(connections),
        Some(crate::pool::MAX_CONNECTIONS as u64),
        "the compiled connection cap is reported"
    );
    assert_eq!(subject_kind(connections), None);
    let sessions = find(ResourceKind::Sessions);
    assert_eq!(sessions.total_used, 1, "the one live session is the count");
    assert_eq!(
        global_limit(sessions),
        Some(7),
        "the configured cap is reported"
    );
    assert_eq!(sessions.unit, ResourceUnit::Count);
    assert_eq!(subject_kind(sessions), None);
    let images = find(ResourceKind::ImageStoreBytes);
    assert_eq!(subject_kind(images), Some(SubjectKind::Session));
    assert_eq!(
        per_subject_limit(images),
        Some(crate::pool::DEFAULT_IMAGE_BYTE_CAP as u64)
    );
    assert_eq!(
        images.total_used, 0,
        "a shell that drew no image holds no bytes"
    );
    assert_eq!(max_subject_used(images), Some(0));
    assert_eq!(find(ResourceKind::InFlightDecodes).total_used, 0);
    assert_eq!(find(ResourceKind::InFlightDecodeBytes).total_used, 0);
    let outbox = find(ResourceKind::SubscriberQueueBytes);
    assert_eq!(subject_kind(outbox), Some(SubjectKind::Subscriber));
    assert_eq!(
        per_subject_limit(outbox),
        Some(session_task::SUBSCRIBER_BUFFER_CAP as u64)
    );
}

/// Drives two samples from opposite ends so a row fed from the wrong source
/// fails rather than coincidentally agreeing, and holds two sessions of
/// different sizes so a sum cannot pass for a deepest subject.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_samples_what_the_sessions_are_holding() {
    use felis_grid::images::{ImageEntry, ImageFormat, ImageId};
    use felis_protocol::messages::ResourceKind;

    const IMAGE_BYTES: usize = 32 * 32 * 4;
    const PARKED: &[u8] = b"aaaabbbb";

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("sleep 30")).await;

    let mut owned = owned_session("sleep 30");
    owned
        .images
        .insert(
            ImageId(9),
            ImageEntry::new(32, 32, ImageFormat::Rgba32, vec![0xAB; IMAGE_BYTES]),
        )
        .expect("insert under cap");
    let head = format!("Ga=T,f=100,m=1;{}", std::str::from_utf8(PARKED).unwrap());
    let cmd = felis_vt::kitty_graphics::parse(head.as_bytes()).expect("a graphics command");
    assert!(
        matches!(
            owned.graphics_reassembler.feed(&cmd),
            felis_vt::kitty_graphics::Outcome::Pending
        ),
        "an m=1 head parks the transmission"
    );
    let image_cap = owned.images.bytes_cap() as u64;
    let deeper = owned.images.bytes_used() as u64;
    let _life = session_task::spawn_owned(
        &pool,
        owned,
        IdlePolicy::default(),
        SessionId::new(),
        0,
        0,
        Vec::new(),
        None,
        Listing::Public,
    )
    .await;

    // Half the pixels of the first: the deepest session must be the
    // other one, so a max reported as a sum fails here.
    let mut smaller = owned_session("sleep 30");
    smaller
        .images
        .insert(
            ImageId(10),
            ImageEntry::new(16, 32, ImageFormat::Rgba32, vec![0xCD; IMAGE_BYTES / 2]),
        )
        .expect("insert under cap");
    let shallower = smaller.images.bytes_used() as u64;
    assert!(
        shallower < deeper,
        "the second session must hold strictly less: {shallower} vs {deeper}"
    );
    let _life2 = session_task::spawn_owned(
        &pool,
        smaller,
        IdlePolicy::default(),
        SessionId::new(),
        0,
        0,
        Vec::new(),
        None,
        Listing::Public,
    )
    .await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    send_request(&mut writer, &OpsToDaemonMsg::Status, 1).await;
    let reply = loop {
        let frame = reader.next_frame().await.unwrap().expect("a Status reply");
        if frame.kind != MessageKind::Ops.as_u16() {
            continue;
        }
        break codec::decode::<OpsToClientMsg>(&frame.body).unwrap();
    };
    let OpsToClientMsg::StatusReply { resources, .. } = reply else {
        panic!("expected OpsToClientMsg::StatusReply, got {reply:?}");
    };
    let find = |kind: ResourceKind| {
        resources
            .iter()
            .find(|r| r.resource == kind)
            .unwrap_or_else(|| panic!("no {kind:?} row in {resources:?}"))
    };

    assert_eq!(find(ResourceKind::Sessions).total_used, 2);
    let images = find(ResourceKind::ImageStoreBytes);
    assert_eq!(
        images.total_used,
        deeper + shallower,
        "the total is every session's store summed: {images:?}"
    );
    assert_eq!(
        max_subject_used(images),
        Some(deeper),
        "the subject sample is the deepest single session, not the sum: {images:?}"
    );
    assert_eq!(
        per_subject_limit(images),
        Some(image_cap),
        "the reported ceiling is the one the store enforces"
    );
    assert_eq!(find(ResourceKind::InFlightDecodes).total_used, 1);
    assert_eq!(
        find(ResourceKind::InFlightDecodeBytes).total_used,
        PARKED.len() as u64
    );
    assert_eq!(
        max_subject_used(find(ResourceKind::InFlightDecodeBytes)),
        Some(PARKED.len() as u64)
    );
}

/// The subscriber row is the one whose halves come from two different
/// `SessionStats` fields (a per-session sum and a per-session max), so
/// four outboxes of distinct depths spread over two sessions pin the
/// split: the deepest subject is a single subscriber, never a session's
/// sum and never the daemon-wide one.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_sums_every_subscriber_backlog_and_names_the_deepest() {
    use felis_protocol::messages::ResourceKind;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // The deepest single outbox is in the second session and is smaller
    // than the first session's two summed, so neither a per-session sum
    // nor the daemon-wide total can pass for the max.
    const DEPTHS: [[usize; 2]; 2] = [[4096, 8192], [16384, 256]];

    let pool = Arc::new(Mutex::new(SessionPool::new()));
    // Held for the whole test: dropping a receiver evicts the
    // subscriber, taking its backlog out of the report.
    let mut attached = Vec::new();
    for depths in DEPTHS {
        let session_task::SessionLifecycle { id, .. } = session_task::spawn_owned(
            &pool,
            owned_session("sleep 30"),
            IdlePolicy::default(),
            SessionId::new(),
            0,
            0,
            Vec::new(),
            None,
            Listing::Public,
        )
        .await;
        let handle = pool
            .lock()
            .await
            .handle_cloned(id)
            .expect("session spawned");
        for depth in depths {
            let (tx, rx) = mpsc::unbounded_channel();
            let buffered = Arc::new(AtomicUsize::new(0));
            let (reply_tx, reply_rx) = oneshot::channel();
            handle
                .cmd
                .send(SessionCmd::Subscribe(SubscribeReq {
                    mode: ConnectionMode::Window,
                    pull_paced: false,
                    live_only: false,
                    tx,
                    buffered: Arc::clone(&buffered),
                    reply: reply_tx,
                }))
                .await
                .expect("the session task accepts the subscribe");
            reply_rx
                .await
                .expect("subscribe reply")
                .expect("the subscribe was admitted");
            // Stamped after the reply, which lands once the rehydrate
            // burst is queued: the gauge is then exactly this depth and
            // nothing drains it, since the receiver is never read.
            buffered.store(depth, Ordering::Relaxed);
            attached.push(rx);
        }
    }

    assert_eq!(
        attached.len(),
        4,
        "every outbox the report must sum is still attached"
    );

    let reply = daemon_status(&pool, &DaemonCaps::default()).await;
    let OpsToClientMsg::StatusReply { resources, .. } = reply else {
        panic!("expected OpsToClientMsg::StatusReply, got {reply:?}");
    };
    let outbox = resources
        .iter()
        .find(|r| r.resource == ResourceKind::SubscriberQueueBytes)
        .expect("a subscriber_queue_bytes row");

    let all: Vec<u64> = DEPTHS.iter().flatten().map(|d| *d as u64).collect();
    assert_eq!(
        outbox.total_used,
        all.iter().sum::<u64>(),
        "the total is every subscriber outbox of every session summed: {outbox:?}"
    );
    assert_eq!(
        max_subject_used(outbox),
        all.iter().copied().max(),
        "the subject sample is the deepest single subscriber: {outbox:?}"
    );
}

// ── Connection admission and handshake deadlines (REQ-916) ──────────

#[cfg(unix)]
fn admission_caps(max_connections: usize, refusal_slots: usize) -> DaemonCaps {
    DaemonCaps {
        admission: ConnectionAdmission::with_refusal_slots(max_connections, refusal_slots),
        ..DaemonCaps::default()
    }
}

/// A permit is released by the connection task's drop, which the peer's
/// close only schedules, so a freed slot has to be polled for.
#[cfg(unix)]
async fn dial_until_admitted(path: &Path) -> bool {
    for _ in 0..200 {
        let (read_half, write_half) = connect(path).await.unwrap();
        let (mut reader, mut writer) = framed(read_half, write_half).await;
        send_kind(
            &mut writer,
            &ConnToDaemonMsg::Hello {
                mode: ConnectionMode::Ops,
                pull_paced: false,
            },
        )
        .await;
        let frame = reader.next_frame().await.unwrap().expect("a Conn reply");
        if matches!(
            codec::decode::<ConnToClientMsg>(&frame.body).unwrap(),
            ConnToClientMsg::Welcome { .. }
        ) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

#[cfg(unix)]
async fn connections_row<R, W>(
    reader: &mut FrameReader<R>,
    writer: &mut FrameWriter<W>,
    request: u64,
) -> felis_protocol::messages::ResourceReport
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    resource_row(
        reader,
        writer,
        request,
        felis_protocol::messages::ResourceKind::Connections,
    )
    .await
}

/// Over-cap peers get a frame, not a bare close: to `felis` and the
/// bridge a closed socket is indistinguishable from a crashed daemon.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dial_past_the_connection_cap_is_refused_at_capacity() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon_with_caps(&path, pool, shell_factory("sleep 30"), admission_caps(1, 4)).await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome_as(&mut reader, &mut writer, ConnectionMode::Ops, false).await;

    let (over_r, over_w) = connect(&path).await.unwrap();
    let (mut over_reader, mut over_writer) = framed(over_r, over_w).await;
    send_kind(
        &mut over_writer,
        &ConnToDaemonMsg::Hello {
            mode: ConnectionMode::Ops,
            pull_paced: false,
        },
    )
    .await;
    let refused = over_reader
        .next_frame()
        .await
        .unwrap()
        .expect("the daemon answers an over-cap dial");
    match codec::decode::<ConnToClientMsg>(&refused.body).unwrap() {
        ConnToClientMsg::Refused {
            reason: RefusalReason::AtCapacity,
            detail,
        } => assert!(
            detail.contains("1 of 1"),
            "the refusal names the count and the ceiling: {detail}"
        ),
        other => panic!("expected Refused(AtCapacity), got {other:?}"),
    }
    drop((over_reader, over_writer));

    drop((reader, writer));
    assert!(
        dial_until_admitted(&path).await,
        "the closed connection must return its permit"
    );
}

/// The refusal path is itself admitted: a flood of silent peers can
/// hold at most `max_connections + refusal slots` tasks and fds.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn silent_over_cap_peers_are_bounded_by_the_refusal_slots() {
    use felis_protocol::preface::ClientPreface;
    use felis_transport::preface::{read_daemon_preface, write_client_preface};

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon_with_caps(&path, pool, shell_factory("sleep 30"), admission_caps(1, 1)).await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome_as(&mut reader, &mut writer, ConnectionMode::Ops, false).await;

    // Its preface reply proves the refusal task is running, so the one
    // refusal slot is taken for as long as this peer stays silent.
    let (silent_r, silent_w) = connect(&path).await.unwrap();
    let (silent_reader, silent_writer) = framed(silent_r, silent_w).await;

    // The close can land before or after the third peer's own write, so
    // both halves count as "unanswered".
    let (mut third_r, mut third_w) = connect(&path).await.unwrap();
    let answered = write_client_preface(&mut third_w, ClientPreface::CURRENT)
        .await
        .is_ok()
        && read_daemon_preface(&mut third_r).await.is_ok();
    assert!(
        !answered,
        "with no refusal slot left the daemon drops the socket unanswered"
    );

    drop((silent_reader, silent_writer));
    drop((reader, writer));
}

/// Every accounted connection is one the daemon is serving, and every
/// permit comes back: the aggregate bound the per-resource ceilings
/// multiply against.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_connections_row_counts_admitted_peers_and_every_permit_returns() {
    const CAP: usize = 4;

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon_with_caps(
        &path,
        pool,
        shell_factory("sleep 30"),
        admission_caps(CAP, 4),
    )
    .await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome_as(&mut reader, &mut writer, ConnectionMode::Ops, false).await;

    let mut held = Vec::new();
    for _ in 1..CAP {
        let (r, w) = connect(&path).await.unwrap();
        let (mut r, mut w) = framed(r, w).await;
        hello_welcome_as(&mut r, &mut w, ConnectionMode::Ops, false).await;
        held.push((r, w));
    }

    assert_eq!(held.len(), CAP - 1);
    let row = connections_row(&mut reader, &mut writer, 1).await;
    assert_eq!(
        row.total_used, CAP as u64,
        "every dial is one admitted peer"
    );
    assert_eq!(global_limit(&row), Some(CAP as u64));

    let (over_r, over_w) = connect(&path).await.unwrap();
    let (mut over_reader, mut over_writer) = framed(over_r, over_w).await;
    send_kind(
        &mut over_writer,
        &ConnToDaemonMsg::Hello {
            mode: ConnectionMode::Ops,
            pull_paced: false,
        },
    )
    .await;
    let refused = over_reader.next_frame().await.unwrap().expect("a refusal");
    assert!(
        matches!(
            codec::decode::<ConnToClientMsg>(&refused.body).unwrap(),
            ConnToClientMsg::Refused {
                reason: RefusalReason::AtCapacity,
                ..
            }
        ),
        "a dial past the cap is refused, never queued"
    );
    drop((over_reader, over_writer));

    drop(held);
    let mut request = 2;
    let settled = loop {
        let row = connections_row(&mut reader, &mut writer, request).await;
        request += 1;
        if row.total_used == 1 || request > 200 {
            break row.total_used;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(
        settled, 1,
        "only the connection asking the question is left holding a permit"
    );
}

/// A peer that opens a socket and writes nothing costs a task and an fd
/// for the preface deadline, not forever.
#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn a_peer_that_writes_no_preface_is_cut_at_the_preface_deadline() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let (client_w, daemon_read) = tokio::io::duplex(64 * 1024);
    let (daemon_write, client_r) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(handle_stream(
        daemon_read,
        daemon_write,
        DaemonCaps::default(),
        pool,
        default_session_factory(),
    ));

    let err = server
        .await
        .unwrap()
        .expect_err("a silent peer must be cut, not awaited");
    assert!(
        matches!(
            err,
            ConnError::HandshakeTimeout {
                phase: HandshakePhase::Preface
            }
        ),
        "{err:?}"
    );
    drop((client_r, client_w));
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn a_peer_that_stops_after_the_preface_is_cut_at_the_hello_deadline() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let (client_w, daemon_read) = tokio::io::duplex(64 * 1024);
    let (daemon_write, client_r) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(handle_stream(
        daemon_read,
        daemon_write,
        DaemonCaps::default(),
        pool,
        default_session_factory(),
    ));

    let (reader, writer) = framed(client_r, client_w).await;
    let err = server.await.unwrap().expect_err("no Hello, no connection");
    assert!(
        matches!(
            err,
            ConnError::HandshakeTimeout {
                phase: HandshakePhase::Hello
            }
        ),
        "{err:?}"
    );
    drop((reader, writer));
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn a_peer_that_never_attaches_is_cut_at_the_first_operation_deadline() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let (client_w, daemon_read) = tokio::io::duplex(64 * 1024);
    let (daemon_write, client_r) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(handle_stream(
        daemon_read,
        daemon_write,
        DaemonCaps::default(),
        pool,
        default_session_factory(),
    ));

    let (mut reader, mut writer) = framed(client_r, client_w).await;
    // A `Window` names its session in the frame after `Welcome`, so
    // silence there is a stall; an `Ops` peer is the exempt one.
    hello_welcome_as(&mut reader, &mut writer, ConnectionMode::Window, false).await;
    let err = server
        .await
        .unwrap()
        .expect_err("a handshaked peer that asks nothing is cut too");
    assert!(
        matches!(
            err,
            ConnError::HandshakeTimeout {
                phase: HandshakePhase::FirstOperation
            }
        ),
        "{err:?}"
    );
    drop((reader, writer));
}

/// `felis bridge` dials its anchor at startup to prove the daemon is
/// reachable and writes nothing until its editor asks for something, so
/// the deadline that cut it would kill an idle bridge for working as
/// documented.
#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn an_ops_connection_may_idle_before_its_first_verb() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let (client_w, daemon_read) = tokio::io::duplex(64 * 1024);
    let (daemon_write, client_r) = tokio::io::duplex(64 * 1024);
    let caps = DaemonCaps::default();
    let idle = caps.handshake.first_op * 3;
    let server = tokio::spawn(handle_stream(
        daemon_read,
        daemon_write,
        caps,
        pool,
        default_session_factory(),
    ));

    let (mut reader, mut writer) = framed(client_r, client_w).await;
    hello_welcome_as(&mut reader, &mut writer, ConnectionMode::Ops, false).await;
    tokio::time::sleep(idle).await;

    send_request(&mut writer, &OpsToDaemonMsg::List, 1).await;
    let frame = reader
        .next_frame()
        .await
        .unwrap()
        .expect("the anchor is still open");
    assert!(matches!(
        codec::decode::<OpsToClientMsg>(&frame.body).unwrap(),
        OpsToClientMsg::Listed { .. }
    ));
    drop((reader, writer));
    drop(server.await);
}

/// Only the *first* operation is timed: a bridge idles between verbs
/// and an observer idles for hours, and cutting either would break the
/// connection modes the daemon exists to serve.
#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn an_ops_connection_that_answered_one_verb_may_idle() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let (client_w, daemon_read) = tokio::io::duplex(64 * 1024);
    let (daemon_write, client_r) = tokio::io::duplex(64 * 1024);
    let caps = DaemonCaps::default();
    let idle = caps.handshake.first_op * 3;
    let server = tokio::spawn(handle_stream(
        daemon_read,
        daemon_write,
        caps,
        pool,
        default_session_factory(),
    ));

    let (mut reader, mut writer) = framed(client_r, client_w).await;
    hello_welcome_as(&mut reader, &mut writer, ConnectionMode::Ops, false).await;
    for request in [1, 2] {
        if request == 2 {
            tokio::time::sleep(idle).await;
        }
        send_request(&mut writer, &OpsToDaemonMsg::List, request).await;
        let frame = reader.next_frame().await.unwrap().expect("a Listed reply");
        assert!(matches!(
            codec::decode::<OpsToClientMsg>(&frame.body).unwrap(),
            OpsToClientMsg::Listed { .. }
        ));
    }
    drop((reader, writer));
    drop(server.await);
}

#[cfg(unix)]
type Peer = (
    FrameReader<felis_transport::local::ReadHalf>,
    FrameWriter<felis_transport::local::WriteHalf>,
);

/// One dial through the handshake, `None` when the daemon is full. Any
/// other answer is a bug in the admission path, so it panics rather
/// than counting as a refusal.
#[cfg(unix)]
async fn try_admit(path: &Path, mode: ConnectionMode) -> Option<Peer> {
    let (read_half, write_half) = connect(path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    send_kind(
        &mut writer,
        &ConnToDaemonMsg::Hello {
            mode,
            pull_paced: false,
        },
    )
    .await;
    let frame = reader.next_frame().await.unwrap().expect("a Conn reply");
    match codec::decode::<ConnToClientMsg>(&frame.body).unwrap() {
        ConnToClientMsg::Welcome { .. } => Some((reader, writer)),
        ConnToClientMsg::Refused {
            reason: RefusalReason::AtCapacity,
            ..
        } => None,
        other => panic!("expected Welcome or Refused(AtCapacity), got {other:?}"),
    }
}

/// The permit rides on the connection's task, and the deadline paths end
/// that task with `Err`. Were the drop not the release, every silent
/// peer would ratchet the daemon's ceiling down by one.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_connection_cut_at_a_handshake_deadline_returns_its_permit() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let caps = DaemonCaps {
        // Long enough to observe the permit being held, short enough
        // that the cut does not pace the test.
        handshake: HandshakeDeadlines {
            hello: Duration::from_millis(500),
            ..HandshakeDeadlines::default()
        },
        ..admission_caps(1, 4)
    };
    spawn_daemon_with_caps(&path, pool, shell_factory("sleep 30"), caps).await;

    // Takes the daemon's only permit and then says nothing, so its
    // handler ends in `Err(HandshakeTimeout { Hello })`.
    let (silent_reader, silent_writer) = {
        let (read_half, write_half) = connect(&path).await.unwrap();
        framed(read_half, write_half).await
    };
    assert!(
        try_admit(&path, ConnectionMode::Ops).await.is_none(),
        "the silent peer holds the only permit while it stalls"
    );

    assert!(
        dial_until_admitted(&path).await,
        "the failing handler must return its permit"
    );
    drop((silent_reader, silent_writer));
}

/// The aggregate bound under the load it exists for: many sessions,
/// several mirrors each, and more dials than the daemon admits. Nothing
/// the load does pushes the admitted count past the ceiling, over-cap
/// dials are typed rather than dropped, and once the peers are gone
/// every permit is back.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_sessions_with_mirrors_never_pass_the_connection_ceiling() {
    const SESSIONS: usize = 16;
    const MIRRORS: usize = 3;
    // Below `SESSIONS * (1 + MIRRORS)`, so the mirrors meet the ceiling
    // instead of fitting under it.
    const CAP: usize = 24;

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon_with_caps(
        &path,
        pool,
        shell_factory("sleep 30"),
        admission_caps(CAP, 8),
    )
    .await;

    // Admitted first and held throughout: every reading of the row is
    // taken by a connection the row itself counts.
    let (mut probe_reader, mut probe_writer) = try_admit(&path, ConnectionMode::Ops)
        .await
        .expect("the first dial of an empty daemon");
    let mut request = 1;

    let mut owners = Vec::new();
    let mut ids = Vec::new();
    for _ in 0..SESSIONS {
        let (mut reader, mut writer) = try_admit(&path, ConnectionMode::Window)
            .await
            .expect("the daemon admits every session's own connection");
        ids.push(create_and_attach(&mut reader, &mut writer).await);
        owners.push((reader, writer));

        let row = connections_row(&mut probe_reader, &mut probe_writer, request).await;
        request += 1;
        assert!(
            row.total_used <= CAP as u64,
            "creating sessions passed the ceiling: {row:?}"
        );
    }

    let mut mirrors = Vec::new();
    let mut refused = 0_usize;
    for id in ids.iter().copied() {
        for _ in 0..MIRRORS {
            match try_admit(&path, ConnectionMode::Window).await {
                Some((reader, mut writer)) => {
                    send_kind(
                        &mut writer,
                        &SessionToDaemonMsg::Attach {
                            target: AttachTarget::Id(id),
                            live_only: false,
                        },
                    )
                    .await;
                    mirrors.push((reader, writer));
                }
                None => refused += 1,
            }
        }
        let row = connections_row(&mut probe_reader, &mut probe_writer, request).await;
        request += 1;
        assert!(
            row.total_used <= CAP as u64,
            "mirroring passed the ceiling: {row:?}"
        );
        assert_eq!(global_limit(&row), Some(CAP as u64));
    }
    assert!(
        refused > 0,
        "the load must actually meet the ceiling, or it proves nothing"
    );

    drop(mirrors);
    drop(owners);
    drop((probe_reader, probe_writer));

    let (mut after_reader, mut after_writer) = loop {
        if let Some(peer) = try_admit(&path, ConnectionMode::Ops).await {
            break peer;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let mut request = 1;
    let settled = loop {
        let row = connections_row(&mut after_reader, &mut after_writer, request).await;
        request += 1;
        if row.total_used == 1 || request > 400 {
            break row.total_used;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(
        settled, 1,
        "after teardown only the asking connection holds a permit"
    );
}

/// The deadline covers the attach, not merely the first frame: the
/// shipped GUI asks `Ops::List` for its roster before it names a
/// session, so a peer that stops after that verb is still a stalled
/// window holding a permit.
#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn a_window_that_only_lists_is_still_cut_at_the_first_operation_deadline() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let (client_w, daemon_read) = tokio::io::duplex(64 * 1024);
    let (daemon_write, client_r) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(handle_stream(
        daemon_read,
        daemon_write,
        DaemonCaps::default(),
        pool,
        default_session_factory(),
    ));

    let (mut reader, mut writer) = framed(client_r, client_w).await;
    hello_welcome_as(&mut reader, &mut writer, ConnectionMode::Window, false).await;
    send_request(&mut writer, &OpsToDaemonMsg::List, 1).await;
    let frame = reader.next_frame().await.unwrap().expect("a Listed reply");
    assert!(matches!(
        codec::decode::<OpsToClientMsg>(&frame.body).unwrap(),
        OpsToClientMsg::Listed { .. }
    ));

    let err = server
        .await
        .unwrap()
        .expect_err("a window that lists but never attaches is cut");
    assert!(
        matches!(
            err,
            ConnError::HandshakeTimeout {
                phase: HandshakePhase::FirstOperation
            }
        ),
        "{err:?}"
    );
    drop((reader, writer));
}

/// The reservation is what bounds the input path, so what it counts is
/// the contract: the post-transform byte count, computed without
/// consulting a mode the child can flip before the write happens.
#[test]
fn a_paste_reserves_its_bracketing_whatever_the_mode() {
    use felis_protocol::limits::{MAX_MOUSE_REPORT_BYTES, MAX_PASTE_BYTES, PASTE_BRACKET_OVERHEAD};
    use felis_protocol::messages::{InputMods, MouseAction, MouseButton, MouseEvent};

    let reserved = |msg: &InputMsg| input_reservation_bytes(msg).map(|n| n.map(usize::try_from));

    assert_eq!(
        reserved(&InputMsg::KeyBytes(b"abc".to_vec())).unwrap(),
        Some(Ok(3)),
        "keystroke bytes reach the child verbatim",
    );
    assert_eq!(
        reserved(&InputMsg::Paste(Vec::new())).unwrap(),
        Some(Ok(PASTE_BRACKET_OVERHEAD)),
        "an empty paste still costs its brackets",
    );
    assert_eq!(
        reserved(&InputMsg::Paste(b"abc".to_vec())).unwrap(),
        Some(Ok(3 + PASTE_BRACKET_OVERHEAD)),
    );
    assert_eq!(
        reserved(&InputMsg::Paste(vec![b'x'; MAX_PASTE_BYTES])).unwrap(),
        Some(Ok(MAX_PASTE_BYTES + PASTE_BRACKET_OVERHEAD)),
        "the largest admitted paste must still fit the budget",
    );
    // A mouse report is client input, not a reply the actor generated:
    // it reaches the PTY, so it is charged like a keystroke (the whole
    // widest encoding, since the actor picks the encoding after this).
    assert_eq!(
        reserved(&InputMsg::Mouse(MouseEvent {
            button: Some(MouseButton::Left),
            action: MouseAction::Press,
            mods: InputMods::empty(),
            x: 1,
            y: 1,
            px: 1,
            py: 1,
        }))
        .unwrap(),
        Some(Ok(MAX_MOUSE_REPORT_BYTES)),
    );
    match input_reservation_bytes(&InputMsg::Paste(vec![b'x'; MAX_PASTE_BYTES + 1])) {
        Err(ConnError::InputOverLimit { bytes, limit }) => {
            assert_eq!((bytes, limit), (MAX_PASTE_BYTES + 1, MAX_PASTE_BYTES));
        }
        other => panic!("an over-limit paste must be refused, got {other:?}"),
    }

    for msg in [
        InputMsg::NextGridFrame,
        InputMsg::FocusChange { focused: true },
        InputMsg::Viewport {
            lines_from_bottom: 3,
        },
    ] {
        assert_eq!(
            input_reservation_bytes(&msg).unwrap(),
            None,
            "{msg:?} reaches no PTY and must not charge the budget",
        );
    }
}

/// A key's encoded width depends on modes the child owns, so the permit
/// covers the widest report any mode can produce and the two per-key
/// caps are enforced before it is asked for.
#[test]
fn a_key_reserves_the_worst_case_report_and_refuses_an_oversized_one() {
    use felis_protocol::limits::{
        MAX_KEY_CHARACTER_BYTES, MAX_KEY_REPORT_BYTES, MAX_KEY_TEXT_BYTES,
    };
    use felis_protocol::messages::{Key, KeyEvent, KeyEventKind, KeyLocation, KeyMods, NamedKey};

    let event = |key: Key, text: Option<String>| {
        InputMsg::Key(KeyEvent {
            key,
            text,
            mods: KeyMods::empty(),
            kind: KeyEventKind::Press,
            location: KeyLocation::Standard,
        })
    };

    assert_eq!(
        input_reservation_bytes(&event(Key::Named(NamedKey::Enter), None))
            .unwrap()
            .map(usize::try_from),
        Some(Ok(MAX_KEY_REPORT_BYTES)),
        "the permit cannot depend on a mode the child can flip after admission",
    );
    assert_eq!(
        input_reservation_bytes(&event(
            Key::Character("a".repeat(MAX_KEY_CHARACTER_BYTES)),
            Some("a".repeat(MAX_KEY_TEXT_BYTES)),
        ))
        .unwrap()
        .map(usize::try_from),
        Some(Ok(MAX_KEY_REPORT_BYTES)),
        "the largest admitted key costs no more than any other",
    );

    for (msg, limit) in [
        (
            event(
                Key::Character("a".repeat(MAX_KEY_CHARACTER_BYTES + 1)),
                None,
            ),
            MAX_KEY_CHARACTER_BYTES,
        ),
        (
            event(
                Key::Named(NamedKey::Enter),
                Some("a".repeat(MAX_KEY_TEXT_BYTES + 1)),
            ),
            MAX_KEY_TEXT_BYTES,
        ),
    ] {
        match input_reservation_bytes(&msg) {
            Err(ConnError::InputOverLimit { bytes, limit: got }) => {
                assert_eq!((bytes, got), (limit + 1, limit));
            }
            other => panic!("an over-limit key must be refused, got {other:?}"),
        }
    }
}

/// Reads until the fence reply lands, discarding the grid traffic that
/// shares the connection. `None` is the connection ending first, by
/// EOF or by the reset a daemon-side close leaves behind.
#[cfg(unix)]
async fn input_accepted<R>(reader: &mut FrameReader<R>) -> Option<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        let frame = reader.next_frame().await.ok().flatten()?;
        if frame.kind == MessageKind::Session.as_u16()
            && matches!(
                codec::decode::<SessionToClientMsg>(&frame.body).unwrap(),
                SessionToClientMsg::InputAccepted
            )
        {
            return Some(());
        }
    }
}

/// A fence with nothing ahead of it is answered on the spot: the
/// barrier is over this connection's own earlier input, not over the
/// session's work.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_fence_is_answered_at_once() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool, shell_factory("read _x")).await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    let _id = create_and_attach(&mut reader, &mut writer).await;

    send_request(&mut writer, &SessionToDaemonMsg::InputFence, 1).await;
    tokio::time::timeout(Duration::from_secs(10), input_accepted(&mut reader))
        .await
        .expect("an empty fence is answered without waiting on anything")
        .expect("the connection stays open");
}

/// What the barrier buys the caller: once the fence is answered the
/// bytes are the daemon's, so a client that hangs up straight after
/// still gets them typed into the child.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_admitted_before_the_fence_survives_the_client_hanging_up() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(
        &path,
        Arc::clone(&pool),
        shell_factory("read line; printf 'GOT:%s' \"$line\""),
    )
    .await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    let id = create_and_attach(&mut reader, &mut writer).await;

    send_input(&mut writer, &InputMsg::Paste(b"felis\r".to_vec())).await;
    send_request(&mut writer, &SessionToDaemonMsg::InputFence, 1).await;
    tokio::time::timeout(Duration::from_secs(30), input_accepted(&mut reader))
        .await
        .expect("the fence behind an admitted paste is answered")
        .expect("the connection stays open");
    drop(writer);
    drop(reader);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let (read_half, write_half) = connect(&path).await.unwrap();
        let (mut watcher, mut watcher_writer) = framed(read_half, write_half).await;
        hello_welcome(&mut watcher, &mut watcher_writer, false).await;
        let rows = attach_existing(&mut watcher, &mut watcher_writer, id).await;
        if rows.iter().any(|row| row.contains("GOT:felis")) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the child never read the admitted paste: {rows:?}",
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The boundary the reference states: a fence sent once the session is
/// gone is answered by the connection ending, never by an
/// acknowledgement of input no session can hold.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fence_after_the_session_ended_gets_no_reply() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, Arc::clone(&pool), shell_factory("read _x")).await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    let id = create_and_attach(&mut reader, &mut writer).await;

    pool.lock()
        .await
        .handle_cloned(SessionId(id))
        .expect("the session is still pooled")
        .cmd
        .send(SessionCmd::Shutdown)
        .await
        .expect("the session task takes the shutdown");
    wait_for_gone(&pool, SessionId(id)).await;

    // The write itself may already meet the closed connection, which is
    // the same verdict the read below gives.
    let _sent = writer
        .send_correlated(
            &SessionToDaemonMsg::InputFence,
            Correlation::request(RequestId::new(1).expect("non-zero request id")),
        )
        .await;
    assert!(
        tokio::time::timeout(Duration::from_secs(10), input_accepted(&mut reader))
            .await
            .expect("the connection ends rather than hanging")
            .is_none(),
        "a fence the session cannot answer ends the connection",
    );
}

/// The barrier `felis sessions send` relies on: the fence it issues
/// after the input cannot be answered while that input is still parked
/// on the budget, so a reply is proof the key was admitted rather than
/// dropped.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_key_parked_on_a_full_budget_holds_back_the_fence_behind_it() {
    use felis_protocol::limits::MAX_PASTE_BYTES;
    use felis_protocol::messages::{Key, KeyEvent, KeyEventKind, KeyLocation, KeyMods, NamedKey};

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    // Reads nothing at first, so the budget-filling paste stays charged
    // and the key behind it cannot be admitted.
    spawn_daemon(
        &path,
        pool,
        shell_factory("stty raw -echo; sleep 3; exec cat >/dev/null"),
    )
    .await;

    let (typist_r, typist_w) = connect(&path).await.unwrap();
    let (mut typist_reader, mut typist_writer) = framed(typist_r, typist_w).await;
    hello_welcome(&mut typist_reader, &mut typist_writer, false).await;
    let _id = create_and_attach(&mut typist_reader, &mut typist_writer).await;

    send_input(
        &mut typist_writer,
        &InputMsg::Paste(vec![b'x'; MAX_PASTE_BYTES]),
    )
    .await;
    send_input(
        &mut typist_writer,
        &InputMsg::Key(KeyEvent {
            key: Key::Named(NamedKey::Enter),
            text: None,
            mods: KeyMods::empty(),
            kind: KeyEventKind::Press,
            location: KeyLocation::Standard,
        }),
    )
    .await;
    send_request(&mut typist_writer, &SessionToDaemonMsg::InputFence, 1).await;

    // Draining the grid stream is what keeps the daemon's outbound half
    // from being the thing that blocks; the fence reply is the frame
    // under test.
    assert!(
        tokio::time::timeout(Duration::from_secs(1), input_accepted(&mut typist_reader))
            .await
            .is_err(),
        "the fence was answered while the key ahead of it was still unadmitted",
    );
    tokio::time::timeout(Duration::from_secs(60), input_accepted(&mut typist_reader))
        .await
        .expect("the fence must be answered once the child drains the budget")
        .expect("the connection stays open");

    drop(typist_writer);
}

/// Verify `MAX_PASTE_BYTES` is grantable in one reservation against the budget.
///
/// Holds almost the entire budget while the child stalls, releases it once drained,
/// and refuses bytes past the cap before hitting the semaphore.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_exact_limit_paste_takes_the_budget_whole_and_completes() {
    use felis_protocol::limits::{MAX_PASTE_BYTES, PASTE_BRACKET_OVERHEAD};

    const RESERVED: u64 = (MAX_PASTE_BYTES + PASTE_BRACKET_OVERHEAD) as u64;

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    // Reads nothing at first, so the writer thread blocks with the whole
    // reservation still held; `cat` then drains all of it. Raw mode for
    // the same reason as the stall test above: a line discipline would
    // discard the tail instead of throttling the master.
    spawn_daemon(
        &path,
        pool,
        shell_factory("stty raw -echo; sleep 3; exec cat >/dev/null"),
    )
    .await;

    let (ops_r, ops_w) = connect(&path).await.unwrap();
    let (mut ops_reader, mut ops_writer) = framed(ops_r, ops_w).await;
    hello_welcome_as(&mut ops_reader, &mut ops_writer, ConnectionMode::Ops, false).await;
    let mut request = 1;

    let (typist_r, typist_w) = connect(&path).await.unwrap();
    let (mut typist_reader, mut typist_writer) = framed(typist_r, typist_w).await;
    hello_welcome(&mut typist_reader, &mut typist_writer, false).await;
    let id = create_and_attach(&mut typist_reader, &mut typist_writer).await;
    let drain =
        tokio::spawn(
            async move { while let Ok(Some(_frame)) = typist_reader.next_frame().await {} },
        );

    send_input(
        &mut typist_writer,
        &InputMsg::Paste(vec![b'x'; MAX_PASTE_BYTES]),
    )
    .await;

    // Held whole: the permit rides the bytes to the writer thread and is
    // dropped only once `write_all` returns, so while the child sleeps
    // the reservation is the entire paste plus its bracketing.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let peak = loop {
        let row = pty_input_row(&mut ops_reader, &mut ops_writer, request).await;
        request += 1;
        if max_subject_used(&row) == Some(RESERVED) {
            break row;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the budget held {} of {RESERVED} bytes",
            max_subject_used(&row).unwrap_or_default(),
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(
        per_subject_limit(&peak)
            .zip(max_subject_used(&peak))
            .is_some_and(|(limit, used)| used <= limit),
        "one paste must fit the budget it is pinned against: {peak:?}",
    );

    // And given back once the child starts reading: this is the half a
    // reservation-size unit test cannot see.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let row = pty_input_row(&mut ops_reader, &mut ops_writer, request).await;
        request += 1;
        if max_subject_used(&row) == Some(0) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the child drained but {} bytes are still reserved",
            max_subject_used(&row).unwrap_or_default(),
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // One byte past the cap is refused before any permit is asked for: a
    // reservation that large could never be granted, so the connection
    // goes rather than parking forever.
    let (over_r, over_w) = connect(&path).await.unwrap();
    let (mut over_reader, mut over_writer) = framed(over_r, over_w).await;
    hello_welcome(&mut over_reader, &mut over_writer, false).await;
    let _rehydrated = attach_existing(&mut over_reader, &mut over_writer, id).await;
    // Written as a raw frame, because `FrameWriter::send` refuses this
    // body itself: the peer under test here is the one that ignored the
    // published limit, which is the only way the daemon's own guard is
    // ever reached.
    let over_limit = codec::encode(&InputMsg::Paste(vec![b'x'; MAX_PASTE_BYTES + 1]));
    over_writer
        .write_frame_unchecked(&Frame {
            kind: MessageKind::Input.as_u16(),
            body: &over_limit,
        })
        .await
        .unwrap();
    over_writer.flush().await.unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(10), async {
        while let Ok(Some(_frame)) = over_reader.next_frame().await {}
    })
    .await;
    assert!(
        closed.is_ok(),
        "an over-limit paste must end its connection"
    );

    let after = pty_input_row(&mut ops_reader, &mut ops_writer, request).await;
    request += 1;
    assert_eq!(
        max_subject_used(&after),
        Some(0),
        "a refused paste must not have charged the budget: {after:?}",
    );
    assert_eq!(
        resource_row(
            &mut ops_reader,
            &mut ops_writer,
            request,
            felis_protocol::messages::ResourceKind::Sessions,
        )
        .await
        .total_used,
        1,
        "the session survives the connection that over-reached",
    );

    drain.abort();
    drop(typist_writer);
}

/// The race the mode-independent reservation exists for: the pump
/// counts a paste's bytes while `?2004` is off, the child turns it on
/// while that paste is parked on the budget, and the actor then writes
/// the bracketed form. The brackets must come out of the reservation
/// that was already granted, not out of a budget nobody charged.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paste_admitted_before_a_mode_flip_is_written_bracketed_within_its_reservation() {
    use felis_protocol::limits::MAX_PASTE_BYTES;

    // Past the 64 bytes the first paste leaves unreserved, so this one
    // cannot be admitted beside it and must park on the budget, which
    // is what puts the mode flip between its admission and its write.
    const MARKER: &str = "FLIPPED-0123456789-0123456789-0123456789-0123456789-\
0123456789-0123456789-0123456789";

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let received = tmp.path().join("received");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    // Reads nothing while the first paste fills the budget and the
    // second parks behind it, *then* enables bracketed paste, and only
    // afterwards drains: the flip therefore lands between the second
    // paste's admission and its write.
    let script = format!(
        "stty raw -echo; sleep 3; printf '\\033[?2004h'; exec cat > {}",
        received.display()
    );
    let factory: SessionFactory = Arc::new(move |_| {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", &script]);
        cmd.env_clear();
        cmd.env("PATH", fixture_path());
        cmd
    });
    spawn_daemon(&path, pool, factory).await;

    let (typist_r, typist_w) = connect(&path).await.unwrap();
    let (mut typist_reader, mut typist_writer) = framed(typist_r, typist_w).await;
    hello_welcome(&mut typist_reader, &mut typist_writer, false).await;
    let _id = create_and_attach(&mut typist_reader, &mut typist_writer).await;
    let drain =
        tokio::spawn(
            async move { while let Ok(Some(_frame)) = typist_reader.next_frame().await {} },
        );

    // Takes the budget whole, under `?2004` off.
    send_input(
        &mut typist_writer,
        &InputMsg::Paste(vec![b'x'; MAX_PASTE_BYTES]),
    )
    .await;
    send_input(
        &mut typist_writer,
        &InputMsg::Paste(MARKER.as_bytes().to_vec()),
    )
    .await;

    let expected = format!("\x1b[200~{MARKER}\x1b[201~");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if std::fs::read(&received).is_ok_and(|bytes| bytes.ends_with(expected.as_bytes())) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the paste admitted before the mode flip never reached the child bracketed",
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    drain.abort();
    drop(typist_writer);
}

/// The acceptance test for the input bound: a child that stopped reading
/// its stdin must stop the connection feeding it, not grow the daemon,
/// and must leave the session actor answering everyone else.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_child_parks_the_typist_and_not_the_daemon() {
    use felis_protocol::messages::{ResourceKind, SubjectKind};

    const CHUNK: usize = 1024 * 1024;
    // Enough to fill the budget plus whatever the socket buffers, with
    // headroom so a slow machine is not what fails the assertion.
    const MAX_CHUNKS: usize = 64;

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    // Child never reads stdin, filling the PTY buffer so writes block.
    // Raw mode prevents canonical line discipline from discarding over-long tails
    // or echoing bytes back through the parser. It writes ticks while refusing reads.
    spawn_daemon(
        &path,
        pool,
        shell_factory("stty raw -echo; while :; do printf 'tick\\r\\n'; sleep 1; done"),
    )
    .await;

    let (typist_r, typist_w) = connect(&path).await.unwrap();
    let (mut typist_reader, mut typist_writer) = framed(typist_r, typist_w).await;
    hello_welcome(&mut typist_reader, &mut typist_writer, false).await;
    let id = create_and_attach(&mut typist_reader, &mut typist_writer).await;
    // The daemon's outbound half must never be what blocks: this test is
    // about the inbound direction.
    let drain =
        tokio::spawn(
            async move { while let Ok(Some(_frame)) = typist_reader.next_frame().await {} },
        );

    let mut sent = 0usize;
    let parked = loop {
        if sent >= MAX_CHUNKS {
            break false;
        }
        let paste = InputMsg::Paste(vec![b'x'; CHUNK]);
        if tokio::time::timeout(
            Duration::from_secs(2),
            send_input(&mut typist_writer, &paste),
        )
        .await
        .is_err()
        {
            break true;
        }
        sent += 1;
    };
    assert!(
        parked,
        "{sent} MiB reached a child reading nothing: the daemon is not bounding input",
    );

    let (ops_r, ops_w) = connect(&path).await.unwrap();
    let (mut ops_reader, mut ops_writer) = framed(ops_r, ops_w).await;
    hello_welcome_as(&mut ops_reader, &mut ops_writer, ConnectionMode::Ops, false).await;
    // The reply is itself the liveness proof: `Status` asks every session
    // task for its stats, so a frozen actor would hang here.
    let row = tokio::time::timeout(
        Duration::from_secs(5),
        pty_input_row(&mut ops_reader, &mut ops_writer, 1),
    )
    .await
    .expect("the session actor answers while a connection is parked");
    assert_eq!(
        per_subject_limit(&row),
        Some(felis_protocol::limits::PTY_INPUT_BUDGET as u64),
    );
    assert_eq!(subject_kind(&row), Some(SubjectKind::Session));
    assert!(
        max_subject_used(&row)
            .is_some_and(|max| max > 0 && per_subject_limit(&row).is_some_and(|cap| max <= cap)),
        "the parked session's own input must be visible and inside its budget: {row:?}",
    );

    let mut request = 2;
    await_connections(&mut ops_reader, &mut ops_writer, &mut request, 2).await;

    // Mirror the session while the typist is parked. The mirror must reach
    // `RehydrateEnd` and receive live deltas to prove diff fan-out does not
    // deadlock or hold permits across awaits.
    let (mirror_r, mirror_w) = connect(&path).await.unwrap();
    let (mut mirror_reader, mut mirror_writer) = framed(mirror_r, mirror_w).await;
    hello_welcome(&mut mirror_reader, &mut mirror_writer, false).await;
    let _rehydrated = tokio::time::timeout(
        Duration::from_secs(15),
        attach_existing(&mut mirror_reader, &mut mirror_writer, id),
    )
    .await
    .expect("a second window may attach while another connection is parked");
    // Past the rehydrate boundary every row is a diff the session task
    // composed and fanned out with the typist still parked.
    expect_row_containing(&mut mirror_reader, "tick").await;

    // And it may leave again: an unsubscribe is answered by the same
    // actor the parked connection is waiting on.
    send_kind(&mut mirror_writer, &SessionToDaemonMsg::Detach).await;
    await_connections(&mut ops_reader, &mut ops_writer, &mut request, 2).await;

    // The parked connection going away must not wedge anything: its
    // pending acquisition is dropped with it, and the slot comes back
    // without waiting for a child that never drains.
    drain.abort();
    drop(typist_writer);
    await_connections(&mut ops_reader, &mut ops_writer, &mut request, 1).await;
    let after = tokio::time::timeout(
        Duration::from_secs(5),
        pty_input_row(&mut ops_reader, &mut ops_writer, request),
    )
    .await
    .expect("the daemon still answers after the parked peer hangs up");
    request += 1;
    assert!(
        per_subject_limit(&after) == per_subject_limit(&row),
        "the budget is a per-session constant: {after:?}",
    );
    assert_eq!(
        resource_row(
            &mut ops_reader,
            &mut ops_writer,
            request,
            ResourceKind::Sessions
        )
        .await
        .total_used,
        1,
        "the session survives its typist",
    );
}

/// Peers disconnecting while parked on the budget must release connection slots.
///
/// Ensures a pump parked awaiting budget notices client disconnects even when
/// child processes produce no output to drive the outbound writer.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_that_hangs_up_while_parked_gives_its_connection_back() {
    use felis_protocol::limits::{PASTE_BRACKET_OVERHEAD, PTY_INPUT_BUDGET};

    const CHUNK: usize = 1024 * 1024;
    const RESERVED: usize = CHUNK + PASTE_BRACKET_OVERHEAD;

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(
        &path,
        pool,
        shell_factory("stty raw -echo; printf raw-ready; sleep 30"),
    )
    .await;

    let (typist_r, typist_w) = connect(&path).await.unwrap();
    let (mut typist_reader, mut typist_writer) = framed(typist_r, typist_w).await;
    hello_welcome(&mut typist_reader, &mut typist_writer, false).await;
    let _id = create_and_attach(&mut typist_reader, &mut typist_writer).await;
    // A paste that beats `stty raw` meets canonical mode, where macOS
    // discards input past `MAX_INPUT` and the write completes, so the
    // budget drains instead of filling.
    expect_row_containing(&mut typist_reader, "raw-ready").await;
    let drain =
        tokio::spawn(
            async move { while let Ok(Some(_frame)) = typist_reader.next_frame().await {} },
        );

    let (ops_r, ops_w) = connect(&path).await.unwrap();
    let (mut ops_reader, mut ops_writer) = framed(ops_r, ops_w).await;
    hello_welcome_as(&mut ops_reader, &mut ops_writer, ConnectionMode::Ops, false).await;
    let mut request = 1;

    // Filled to just under the budget, one admitted chunk at a time, so
    // the hangup below is the clean kind: the daemon has taken every
    // byte this peer wrote, which is what a `felis sessions send` that
    // finishes and exits leaves behind.
    let mut admitted = 0usize;
    while admitted + RESERVED <= PTY_INPUT_BUDGET {
        send_input(&mut typist_writer, &InputMsg::Paste(vec![b'x'; CHUNK])).await;
        admitted += RESERVED;
        let want = admitted as u64;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let row = pty_input_row(&mut ops_reader, &mut ops_writer, request).await;
            request += 1;
            if max_subject_used(&row).is_some_and(|used| used >= want) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the daemon admitted {} of {want} bytes",
                max_subject_used(&row).unwrap_or_default(),
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    // One more than the budget can grant: the pump reads it whole and
    // then parks, leaving nothing of this peer's unread on the socket.
    send_input(&mut typist_writer, &InputMsg::Paste(vec![b'x'; CHUNK])).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let parked = pty_input_row(&mut ops_reader, &mut ops_writer, request).await;
    request += 1;
    assert!(
        max_subject_used(&parked).is_some_and(|used| used > (PTY_INPUT_BUDGET - RESERVED) as u64),
        "the last chunk cannot have been admitted: {parked:?}",
    );
    await_connections(&mut ops_reader, &mut ops_writer, &mut request, 2).await;

    drain.abort();
    drop(typist_writer);
    await_connections(&mut ops_reader, &mut ops_writer, &mut request, 1).await;
}

/// The other half of the deadlock surface: a session destroyed under a
/// connection that is parked on its input budget. The parked pump waits
/// on a semaphore no one will post, so the actor's end has to be what
/// releases it.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn destroying_a_session_releases_a_connection_parked_on_its_budget() {
    use felis_protocol::messages::ResolvedId;

    const CHUNK: usize = 1024 * 1024;
    const MAX_CHUNKS: usize = 64;

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool, shell_factory("stty raw -echo; sleep 30")).await;

    let (typist_r, typist_w) = connect(&path).await.unwrap();
    let (mut typist_reader, mut typist_writer) = framed(typist_r, typist_w).await;
    hello_welcome(&mut typist_reader, &mut typist_writer, false).await;
    let id = create_and_attach(&mut typist_reader, &mut typist_writer).await;
    let drain =
        tokio::spawn(
            async move { while let Ok(Some(_frame)) = typist_reader.next_frame().await {} },
        );

    let mut sent = 0usize;
    let parked = loop {
        if sent >= MAX_CHUNKS {
            break false;
        }
        let paste = InputMsg::Paste(vec![b'x'; CHUNK]);
        if tokio::time::timeout(
            Duration::from_secs(2),
            send_input(&mut typist_writer, &paste),
        )
        .await
        .is_err()
        {
            break true;
        }
        sent += 1;
    };
    assert!(parked, "{sent} MiB reached a child reading nothing");

    let (ops_r, ops_w) = connect(&path).await.unwrap();
    let (mut ops_reader, mut ops_writer) = framed(ops_r, ops_w).await;
    hello_welcome_as(&mut ops_reader, &mut ops_writer, ConnectionMode::Ops, false).await;
    send_request(
        &mut ops_writer,
        &OpsToDaemonMsg::Destroy {
            id_prefix: format!("{id:032x}"),
        },
        1,
    )
    .await;
    let reply = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let frame = ops_reader.next_frame().await.unwrap().expect("a reply");
            if frame.kind == MessageKind::Ops.as_u16() {
                break codec::decode::<OpsToClientMsg>(&frame.body).unwrap();
            }
        }
    })
    .await
    .expect("a destroy must not wait for a stalled child");
    assert!(
        matches!(
            reply,
            OpsToClientMsg::Destroyed {
                resolved: ResolvedId::Ok { .. }
            }
        ),
        "{reply:?}",
    );

    // The parked pump has to end with the session it was feeding: the
    // budget it waits on is a semaphore nothing will post once the
    // actor that would have drained the writes is gone. The drain task
    // ends when the daemon closes the connection.
    tokio::time::timeout(Duration::from_secs(10), drain)
        .await
        .expect("the parked connection must be released by the session's end")
        .expect("the drain task must not panic");
    drop(typist_writer);
}

/// Polls until the daemon reports `want` served connections, counting
/// the one asking. Teardown is a task ending, not a reply, so the count
/// it frees lands some time after whatever caused it.
#[cfg(unix)]
async fn await_connections<R, W>(
    reader: &mut FrameReader<R>,
    writer: &mut FrameWriter<W>,
    request: &mut u64,
    want: u64,
) where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use felis_protocol::messages::ResourceKind;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut seen = u64::MAX;
    while tokio::time::Instant::now() < deadline {
        seen = resource_row(reader, writer, *request, ResourceKind::Connections)
            .await
            .total_used;
        *request += 1;
        if seen == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the daemon still serves {seen} connections, expected {want}");
}

#[cfg(unix)]
async fn pty_input_row<R, W>(
    reader: &mut FrameReader<R>,
    writer: &mut FrameWriter<W>,
    request: u64,
) -> felis_protocol::messages::ResourceReport
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    resource_row(
        reader,
        writer,
        request,
        felis_protocol::messages::ResourceKind::PtyInputBytes,
    )
    .await
}

#[cfg(unix)]
async fn resource_row<R, W>(
    reader: &mut FrameReader<R>,
    writer: &mut FrameWriter<W>,
    request: u64,
    kind: felis_protocol::messages::ResourceKind,
) -> felis_protocol::messages::ResourceReport
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    send_request(writer, &OpsToDaemonMsg::Status, request).await;
    let reply = loop {
        let frame = reader.next_frame().await.unwrap().expect("a Status reply");
        if frame.kind != MessageKind::Ops.as_u16() {
            continue;
        }
        break codec::decode::<OpsToClientMsg>(&frame.body).unwrap();
    };
    let OpsToClientMsg::StatusReply { resources, .. } = reply else {
        panic!("expected OpsToClientMsg::StatusReply, got {reply:?}");
    };
    *resources
        .iter()
        .find(|r| r.resource == kind)
        .unwrap_or_else(|| panic!("no {kind:?} row in {resources:?}"))
}

/// The cap is checked against registrations plus the slots creates in
/// flight already hold (REQ-915), and a refusal names that number, so
/// the row must count the same thing: a burst that fills the cap with
/// reservations would otherwise print room the next create is refused
/// for.
#[tokio::test]
async fn the_sessions_row_counts_a_slot_reserved_before_registration() {
    use felis_protocol::messages::ResourceKind;

    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let caps = DaemonCaps {
        max_sessions: 3,
        ..DaemonCaps::default()
    };
    let sessions_row = async |pool: &Arc<Mutex<SessionPool>>| {
        let reply = daemon_status(pool, &caps).await;
        let OpsToClientMsg::StatusReply { resources, .. } = reply else {
            panic!("expected OpsToClientMsg::StatusReply, got {reply:?}");
        };
        *resources
            .iter()
            .find(|r| r.resource == ResourceKind::Sessions)
            .unwrap_or_else(|| panic!("no sessions row in {resources:?}"))
    };

    let slot = pool
        .lock()
        .await
        .try_reserve(caps.max_sessions)
        .expect("a free slot");
    let reserved = sessions_row(&pool).await;
    assert_eq!(
        reserved.total_used, 1,
        "a create between reservation and registration is admitted: {reserved:?}"
    );
    assert_eq!(global_limit(&reserved), Some(3));

    drop(slot);
    let released = sessions_row(&pool).await;
    assert_eq!(
        released.total_used, 0,
        "a create that never registered gives its slot back: {released:?}"
    );
}

/// `spawn_daemon_with_caps` with the serve future kept, for the stop
/// tests: the daemon exiting is the thing under test.
#[cfg(unix)]
async fn spawn_daemon_watching(
    path: &Path,
    pool: Arc<Mutex<SessionPool>>,
    factory: SessionFactory,
    caps: DaemonCaps,
) -> tokio::task::JoinHandle<Result<(), ServeError>> {
    let server_path = path.to_path_buf();
    let serving =
        tokio::spawn(
            async move { serve_unix_with_factory(&server_path, caps, pool, factory).await },
        );
    for _ in 0..100 {
        if path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(path.exists());
    serving
}

#[cfg(unix)]
async fn stop_reply<R, W>(
    reader: &mut FrameReader<R>,
    writer: &mut FrameWriter<W>,
    mode: StopMode,
    request: u64,
) -> StopOutcome
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    send_request(writer, &OpsToDaemonMsg::Stop { mode }, request).await;
    loop {
        let frame = reader.next_frame().await.unwrap().expect("a Stop reply");
        if frame.kind != MessageKind::Ops.as_u16() {
            continue;
        }
        match codec::decode::<OpsToClientMsg>(&frame.body).unwrap() {
            OpsToClientMsg::StopReply { outcome } => return outcome,
            other => panic!("expected OpsToClientMsg::StopReply, got {other:?}"),
        }
    }
}

/// An `Ops`-mode connection past both handshakes.
#[cfg(unix)]
macro_rules! ops_connection {
    ($path:expr) => {{
        let (read_half, write_half) = connect($path).await.unwrap();
        let (mut reader, mut writer) = framed(read_half, write_half).await;
        hello_welcome_as(&mut reader, &mut writer, ConnectionMode::Ops, false).await;
        (reader, writer)
    }};
}

/// A bare `stop` is not allowed to destroy anything: it reports the
/// count and leaves the daemon serving.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_default_stop_refuses_while_a_session_remains_and_preserves_it() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let serving = spawn_daemon_watching(
        &path,
        pool.clone(),
        shell_factory("sleep 30"),
        DaemonCaps::default(),
    )
    .await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    create_and_attach(&mut reader, &mut writer).await;

    let (mut ops_reader, mut ops_writer) = ops_connection!(&path);
    assert_eq!(
        stop_reply(&mut ops_reader, &mut ops_writer, StopMode::IfEmpty, 1).await,
        StopOutcome::Refused { sessions: 1 }
    );
    {
        let guard = pool.lock().await;
        assert_eq!(guard.len(), 1, "the refused stop touched no session");
        assert!(!guard.draining(), "a refused stop starts no drain");
    }
    // Still serving: a connection opened after the refusal is admitted.
    let (mut after_reader, mut after_writer) = ops_connection!(&path);
    send_request(&mut after_writer, &OpsToDaemonMsg::List, 1).await;
    after_reader.next_frame().await.unwrap().expect("a roster");
    assert!(!serving.is_finished());
    serving.abort();
}

/// `--force` destroys every session, and the process it was asked to
/// stop actually goes: the accept loop returns and the socket path is
/// cleared.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forced_stop_destroys_every_session_and_ends_the_accept_loop() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let serving = spawn_daemon_watching(
        &path,
        pool.clone(),
        shell_factory("sleep 30"),
        DaemonCaps::default(),
    )
    .await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    create_and_attach(&mut reader, &mut writer).await;

    let (mut ops_reader, mut ops_writer) = ops_connection!(&path);
    assert_eq!(
        stop_reply(&mut ops_reader, &mut ops_writer, StopMode::Force, 1).await,
        StopOutcome::Stopping,
        "the reply reaches the caller before the daemon goes"
    );
    assert_eq!(pool.lock().await.len(), 0, "force destroys every session");
    tokio::time::timeout(Duration::from_secs(20), serving)
        .await
        .expect("the daemon exits on a forced stop")
        .expect("the serve task did not panic")
        .expect("the accept loop ends cleanly");
    // REQ-009d: exit unlinks nothing, so an old daemon dying can never
    // remove a newer one's socket; the next starter's probe replaces it.
    assert!(
        std::fs::symlink_metadata(&path).is_ok(),
        "the stopped daemon leaves its socket path in place"
    );
    assert_eq!(
        std::os::unix::net::UnixStream::connect(&path)
            .unwrap_err()
            .kind(),
        io::ErrorKind::ConnectionRefused,
        "and nothing answers there any more"
    );
}

/// `--when-empty` is the drain: typed and observable while it lasts,
/// refusing creates, and ending the daemon after the last session.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_when_empty_stop_refuses_creates_and_exits_after_the_last_session() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let serving = spawn_daemon_watching(
        &path,
        pool.clone(),
        shell_factory("sleep 30"),
        DaemonCaps::default(),
    )
    .await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    let id = create_and_attach(&mut reader, &mut writer).await;

    let (mut ops_reader, mut ops_writer) = ops_connection!(&path);
    assert_eq!(
        stop_reply(&mut ops_reader, &mut ops_writer, StopMode::WhenEmpty, 1).await,
        StopOutcome::Draining { sessions: 1 }
    );
    // Idempotent: a second stop while draining answers the same state.
    assert_eq!(
        stop_reply(&mut ops_reader, &mut ops_writer, StopMode::IfEmpty, 2).await,
        StopOutcome::Draining { sessions: 1 }
    );

    // The state is observable in `Ops::Status`.
    send_request(&mut ops_writer, &OpsToDaemonMsg::Status, 3).await;
    let status = loop {
        let frame = ops_reader.next_frame().await.unwrap().expect("a status");
        if frame.kind != MessageKind::Ops.as_u16() {
            continue;
        }
        break codec::decode::<OpsToClientMsg>(&frame.body).unwrap();
    };
    match status {
        OpsToClientMsg::StatusReply { draining, .. } => assert!(draining),
        other => panic!("expected a StatusReply, got {other:?}"),
    }

    // And a create arriving during the drain is refused with a reason
    // that names it.
    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut new_reader, mut new_writer) = framed(read_half, write_half).await;
    hello_welcome(&mut new_reader, &mut new_writer, false).await;
    send_kind(
        &mut new_writer,
        &SessionToDaemonMsg::Create {
            args: felis_protocol::messages::SpawnArgs::default(),
        },
    )
    .await;
    let refused = new_reader.next_frame().await.unwrap().expect("a refusal");
    match codec::decode::<SessionToClientMsg>(&refused.body).unwrap() {
        SessionToClientMsg::AttachFailed { reason, detail } => {
            assert_eq!(reason, AttachRefusal::Create(CreateFailure::DaemonDraining));
            assert!(detail.contains("draining"), "{detail}");
        }
        other => panic!("expected AttachFailed, got {other:?}"),
    }
    assert_eq!(pool.lock().await.len(), 1, "the drain destroyed nothing");

    send_request(
        &mut ops_writer,
        &OpsToDaemonMsg::Destroy {
            id_prefix: format!("{}", felis_protocol::SessionHex(id)),
        },
        4,
    )
    .await;
    tokio::time::timeout(Duration::from_secs(20), serving)
        .await
        .expect("the daemon exits once its last session ends")
        .expect("the serve task did not panic")
        .expect("the accept loop ends cleanly");
    // REQ-009d: exit unlinks nothing, so an old daemon dying can never
    // remove a newer one's socket; the next starter's probe replaces it.
    assert!(
        std::fs::symlink_metadata(&path).is_ok(),
        "the stopped daemon leaves its socket path in place"
    );
    assert_eq!(
        std::os::unix::net::UnixStream::connect(&path)
            .unwrap_err()
            .kind(),
        io::ErrorKind::ConnectionRefused,
        "and nothing answers there any more"
    );
}

/// The emptiness decision cannot race a create: a reservation taken and
/// not yet registered is what a create in flight *is*, so it counts as
/// non-empty and holds the drain open, and once a stop has been
/// answered no further reservation is admitted at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_create_in_flight_counts_as_non_empty_and_holds_the_drain_open() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let caps = DaemonCaps::default();
    let slot = pool
        .lock()
        .await
        .try_reserve(8)
        .expect("an empty pool admits a create");

    assert_eq!(
        stop_daemon(&pool, &caps, StopMode::IfEmpty).await,
        StopOutcome::Refused { sessions: 1 },
        "a create between its reservation and its registration is a session"
    );
    assert_eq!(
        stop_daemon(&pool, &caps, StopMode::WhenEmpty).await,
        StopOutcome::Draining { sessions: 1 }
    );
    assert!(!caps.shutdown.fired(), "the drain is not settled yet");
    assert_eq!(
        pool.lock().await.try_reserve(8).unwrap_err(),
        ReserveRefusal::Draining,
        "no create is admitted after a stop was answered"
    );

    // The create it belonged to failed, which resolves the reservation.
    drop(slot);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !caps.shutdown.fired() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the drain settles when the last reservation resolves");
}

/// A session leaves the pool before its child is hung up and reaped,
/// on the panic path from a task that outlives the session's own, so
/// an empty pool is not yet a daemon that may exit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_waits_for_a_child_still_being_reaped_after_its_session_left() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let caps = DaemonCaps::default();
    let teardown = pool.lock().await.begin_teardown();

    let stopping = tokio::spawn({
        let pool = Arc::clone(&pool);
        let caps = caps.clone();
        async move { stop_daemon(&pool, &caps, StopMode::WhenEmpty).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !stopping.is_finished(),
        "the stop is not answered while a child is still being reaped"
    );

    drop(teardown);
    let outcome = tokio::time::timeout(Duration::from_secs(5), stopping)
        .await
        .expect("the stop settles once the reap is done")
        .expect("the stop task did not panic");
    assert_eq!(outcome, StopOutcome::Stopping);
}

/// The shutdown does not ride on the reply reaching anyone: a requester
/// that vanishes right after sending `Stop` still leaves a daemon whose
/// pool it already emptied, so that daemon must exit anyway.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_whose_reply_cannot_be_written_still_stops_the_daemon() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let serving = spawn_daemon_watching(
        &path,
        pool.clone(),
        shell_factory("sleep 30"),
        DaemonCaps::default(),
    )
    .await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    create_and_attach(&mut reader, &mut writer).await;

    let (ops_reader, mut ops_writer) = ops_connection!(&path);
    send_request(
        &mut ops_writer,
        &OpsToDaemonMsg::Stop {
            mode: StopMode::Force,
        },
        1,
    )
    .await;
    drop(ops_reader);
    drop(ops_writer);

    tokio::time::timeout(Duration::from_secs(20), serving)
        .await
        .expect("the daemon exits even when the stop reply cannot be delivered")
        .expect("the serve task did not panic")
        .expect("the accept loop ends cleanly");
}

#[cfg(unix)]
async fn attach_failure<R>(reader: &mut FrameReader<R>) -> (AttachRefusal, String)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let frame = reader.next_frame().await.unwrap().expect("a refusal");
    match codec::decode::<SessionToClientMsg>(&frame.body).unwrap() {
        SessionToClientMsg::AttachFailed { reason, detail } => (reason, detail),
        other => panic!("expected SessionToClientMsg::AttachFailed, got {other:?}"),
    }
}

#[cfg(unix)]
async fn info_outcome<R>(reader: &mut FrameReader<R>) -> InfoOutcome
where
    R: tokio::io::AsyncRead + Unpin,
{
    let frame = reader.next_frame().await.unwrap().expect("an InfoReply");
    match codec::decode::<OpsToClientMsg>(&frame.body).unwrap() {
        OpsToClientMsg::InfoReply { outcome } => outcome,
        other => panic!("expected OpsToClientMsg::InfoReply, got {other:?}"),
    }
}

#[cfg(unix)]
async fn pooled_session(pool: &Arc<Mutex<SessionPool>>) -> SessionId {
    let session_task::SessionLifecycle { id, .. } = session_task::spawn_owned(
        pool,
        owned_session("sleep 30"),
        IdlePolicy::default(),
        SessionId::new(),
        0,
        0,
        Vec::new(),
        None,
        Listing::Public,
    )
    .await;
    id
}

/// Session ids are random, so an ambiguous prefix has to be found
/// rather than written down: sessions are added until two of them share
/// a leading hex digit, which sixteen possible digits make quick.
#[cfg(unix)]
async fn prefix_matching_several(pool: &Arc<Mutex<SessionPool>>) -> (String, u32) {
    let mut ids: Vec<u128> = Vec::new();
    for _ in 0..24 {
        ids.push(pooled_session(pool).await.0);
        let mut counts = std::collections::BTreeMap::<char, u32>::new();
        for id in &ids {
            let hex = felis_protocol::SessionHex(*id).to_string();
            let first = hex.chars().next().expect("a 32-digit rendering");
            *counts.entry(first).or_default() += 1;
        }
        if let Some((digit, matches)) = counts.iter().find(|(_, matches)| **matches > 1) {
            return (digit.to_string(), *matches);
        }
    }
    panic!("24 random ids shared no leading hex digit");
}

/// A prefix names a session the daemon resolves for itself, and the ack
/// carries the full id back.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attach_by_prefix_lands_on_the_session_the_daemon_resolves() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("sleep 30")).await;
    let id = pooled_session(&pool).await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    let hex = felis_protocol::SessionHex(id.0).to_string();
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Attach {
            target: AttachTarget::Prefix(hex[..8].to_owned()),
            live_only: false,
        },
    )
    .await;
    assert_eq!(attached_info(&mut reader).await.id, id.0);
}

/// The resolution and the handle are taken under one lock, so a session
/// destroyed before the attach arrives is a `NoMatch` on the prefix
/// rather than an attach to a row the pool has dropped.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prefix_whose_session_was_destroyed_refuses_with_no_match() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("sleep 30")).await;
    let doomed = pooled_session(&pool).await;
    let hex = felis_protocol::SessionHex(doomed.0).to_string();

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome_as(&mut reader, &mut writer, ConnectionMode::Ops, false).await;
    send_request(
        &mut writer,
        &OpsToDaemonMsg::Destroy {
            id_prefix: hex[..8].to_owned(),
        },
        1,
    )
    .await;
    let destroyed = reader.next_frame().await.unwrap().expect("a Destroyed");
    assert!(matches!(
        codec::decode::<OpsToClientMsg>(&destroyed.body).unwrap(),
        OpsToClientMsg::Destroyed {
            resolved: ResolvedId::Ok { .. }
        }
    ));

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Attach {
            target: AttachTarget::Prefix(hex[..8].to_owned()),
            live_only: false,
        },
    )
    .await;
    assert_eq!(
        attach_failure(&mut reader).await.0,
        AttachRefusal::Attach(AttachFailure::NoMatch)
    );
}

/// A prefix short enough to name two sessions is refused by count, not
/// resolved to whichever the pool happened to walk last.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ambiguous_prefix_on_attach_refuses_with_the_match_count() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("sleep 30")).await;
    let (prefix, matches) = prefix_matching_several(&pool).await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Attach {
            target: AttachTarget::Prefix(prefix),
            live_only: false,
        },
    )
    .await;
    let (reason, detail) = attach_failure(&mut reader).await;
    assert_eq!(reason, AttachRefusal::Attach(AttachFailure::Ambiguous));
    assert!(
        detail.contains(&matches.to_string()),
        "the match count rides in detail: {detail}"
    );
}

/// `Ops::Info` answers the row and the display prefix from one pool
/// snapshot, and the prefix it reports resolves back to the same
/// session.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ops_info_answers_a_row_and_a_short_id_checked_against_the_pool() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("sleep 30")).await;
    let first = pooled_session(&pool).await;
    let second = pooled_session(&pool).await;
    let hex = felis_protocol::SessionHex(first.0).to_string();

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    send_request(
        &mut writer,
        &OpsToDaemonMsg::Info {
            id_prefix: hex[..16].to_owned(),
        },
        1,
    )
    .await;
    let InfoOutcome::Found { session, short_id } = info_outcome(&mut reader).await else {
        panic!("the prefix names exactly one session");
    };
    assert_eq!(session.id, first.0);
    assert_eq!(
        felis_protocol::session_prefix::resolve_session_prefix(&short_id, [first.0, second.0]),
        ResolvedId::Ok { id: first.0 },
        "the reported short id must resolve against the pool it was shortened against"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ops_info_reports_no_match_and_ambiguity_as_outcome_arms() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("sleep 30")).await;
    let (prefix, matches) = prefix_matching_several(&pool).await;

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    send_request(
        &mut writer,
        &OpsToDaemonMsg::Info {
            id_prefix: "f".repeat(32),
        },
        1,
    )
    .await;
    assert_eq!(info_outcome(&mut reader).await, InfoOutcome::NoMatch);

    send_request(&mut writer, &OpsToDaemonMsg::Info { id_prefix: prefix }, 2).await;
    assert_eq!(
        info_outcome(&mut reader).await,
        InfoOutcome::Ambiguous { matches }
    );
}

/// One step of the mirror property below: the parse, the drain that
/// parks its scroll effects, a fan-out cycle composing for some
/// subscribers, a resize, and a subscriber browsing scrollback.
#[derive(Debug, Clone)]
enum MirrorStep {
    Feed(Vec<u8>),
    Drain,
    Cycle([bool; 2]),
    Resize(u16, u16),
    Browse(usize, u32),
}

fn mirror_token() -> impl proptest::strategy::Strategy<Value = Vec<u8>> {
    use proptest::prelude::*;
    prop_oneof![
        4 => proptest::collection::vec(b'a'..=b'z', 1..6),
        3 => Just(b"\r\n".to_vec()),
        1 => Just(b"\n".to_vec()),
        1 => Just("\u{3042}".as_bytes().to_vec()),
        1 => Just("e\u{301}".as_bytes().to_vec()),
        1 => Just("\u{202E}".as_bytes().to_vec()),
        1 => Just(b"\x1bM".to_vec()),
        1 => Just(b"\x1bD".to_vec()),
        1 => (1u8..=4, prop_oneof![Just('S'), Just('T'), Just('L'), Just('M'), Just('@'), Just('P')])
            .prop_map(|(n, op)| format!("\x1b[{n}{op}").into_bytes()),
        1 => (1u8..=8, 1u8..=8).prop_map(|(t, b)| format!("\x1b[{t};{b}r").into_bytes()),
        1 => Just(b"\x1b[r".to_vec()),
        1 => (1u8..=8, 1u8..=10).prop_map(|(r, c)| format!("\x1b[{r};{c}H").into_bytes()),
        1 => (0u8..=2).prop_map(|n| format!("\x1b[{n}J").into_bytes()),
        1 => (0u8..=2).prop_map(|n| format!("\x1b[{n}K").into_bytes()),
        1 => prop_oneof![Just(0u8), Just(1), Just(7), 41u8..=43]
            .prop_map(|n| format!("\x1b[{n}m").into_bytes()),
        1 => Just(b"\x1b[?1049h".to_vec()),
        1 => Just(b"\x1b[?1049l".to_vec()),
        1 => (1u8..=5, 2u8..=10)
            .prop_map(|(l, r)| format!("\x1b[?69h\x1b[{l};{r}s").into_bytes()),
        1 => Just(b"\x1b[?69l".to_vec()),
        1 => Just(b"\x1b[?6h".to_vec()),
        1 => Just(b"\x1b[?6l".to_vec()),
        1 => Just(b"\x1b[3J".to_vec()),
        1 => Just(b"\x1b]8;;https://x\x1b\\ln\x1b]8;;\x1b\\".to_vec()),
        2 => prop_oneof![
            Just(b"\r".to_vec()),
            Just(b"\x08".to_vec()),
            Just(b"\t".to_vec()),
            Just(b"\x1b7".to_vec()),
            Just(b"\x1b8".to_vec()),
        ],
        2 => (1u8..=9, prop_oneof![Just('A'), Just('B'), Just('C'), Just('D'), Just('G'), Just('d')])
            .prop_map(|(n, op)| format!("\x1b[{n}{op}").into_bytes()),
        1 => prop_oneof![Just(41u16), Just(45), Just(1045)]
            .prop_map(|m| format!("\x1b[?{m}h").into_bytes()),
        1 => Just(b"\x1b[?25l".to_vec()),
        1 => Just(b"\x1b[?25h".to_vec()),
    ]
}

fn mirror_step() -> impl proptest::strategy::Strategy<Value = MirrorStep> {
    use proptest::prelude::*;
    prop_oneof![
        6 => proptest::collection::vec(mirror_token(), 1..12)
            .prop_map(|tokens| MirrorStep::Feed(tokens.concat())),
        3 => Just(MirrorStep::Drain),
        3 => any::<[bool; 2]>().prop_map(MirrorStep::Cycle),
        1 => (2u16..=8, 2u16..=10).prop_map(|(rows, cols)| MirrorStep::Resize(rows, cols)),
        1 => (0usize..2, prop_oneof![Just(0u32), 1u32..=6])
            .prop_map(|(sub, lines)| MirrorStep::Browse(sub, lines)),
    ]
}

/// The daemon's half of the stream (grid, parked scrolls, one
/// `SubscriberStream` per mirror) wired to real client shadows the way
/// `session_task` wires them, minus the sockets.
struct MirrorRig {
    grid: Grid,
    parser: felis_vt::Parser,
    parked: Vec<streaming::QueuedScroll>,
    subs: Vec<(SubscriberStream, felis_client_core::ShadowScreen)>,
    rows: RowEncodeCache,
}

impl MirrorRig {
    fn new(rows: u16, cols: u16) -> Self {
        let grid = Grid::new(rows, cols);
        let subs = (0..2)
            .map(|_| {
                let mut out = Vec::new();
                let stream = seeded_stream_into(&grid, &mut out);
                let mut shadow = felis_client_core::ShadowScreen::new(rows, cols);
                Self::apply(&mut shadow, &out);
                (stream, shadow)
            })
            .collect();
        Self {
            grid,
            parser: felis_vt::Parser::new(),
            parked: Vec::new(),
            subs,
            rows: RowEncodeCache::default(),
        }
    }

    /// Entries apply one at a time so each can first be checked against
    /// the released client, whose row write compared the incoming cells
    /// with the row's storage past its watermark and skipped the write
    /// on a match, leaving the row reading blank.
    fn apply(shadow: &mut felis_client_core::ShadowScreen, out: &[OutEvent]) {
        for ev in out {
            let OutEvent::Grid(msg) = ev else {
                continue;
            };
            let GridMsg::RowDelta { rows } = msg else {
                shadow.apply(msg).expect("the mirror admits every frame");
                continue;
            };
            for (row, payload) in rows {
                let screen = shadow.screen();
                let mut styles = screen.style_table().clone();
                let decoded = felis_grid::decode_row(&payload.0, &mut styles).expect("decodes");
                if screen.cols() as usize == decoded.cells.len() {
                    let storage = screen.row_cells(*row).unwrap_or(&[]);
                    let read: Vec<felis_grid::Cell> = (0..screen.cols())
                        .map(|c| screen.cell(*row, c).copied().unwrap_or_default())
                        .collect();
                    assert!(
                        storage != decoded.cells.as_slice() || read == decoded.cells,
                        "a released client would skip row {row} and read it blank",
                    );
                }
                shadow
                    .apply(&GridMsg::RowDelta {
                        rows: vec![(*row, payload.clone())],
                    })
                    .expect("the mirror admits every frame");
            }
        }
    }

    fn drain(&mut self) {
        for effect in self.grid.take_pty_effects() {
            if let PtyEffect::Scrolled {
                op,
                geometry_gen,
                first_seq,
                last_seq,
            } = effect
            {
                self.parked.push(streaming::QueuedScroll {
                    geometry_gen,
                    first_seq,
                    last_seq,
                    op,
                });
            }
        }
    }

    fn fan_out(&mut self) {
        let (current_gen, rows) = (self.grid.geometry_gen(), self.grid.rows());
        for queued in std::mem::take(&mut self.parked) {
            for (stream, _) in &mut self.subs {
                stream.offer_scroll(&queued, current_gen, rows);
            }
        }
        for (stream, _) in &mut self.subs {
            stream.catch_up_to(&self.grid);
            stream.damage.merge(self.grid.damage());
        }
        self.grid.damage_mut().clear();
    }

    fn compose(&mut self, i: usize) -> Vec<OutEvent> {
        let mut out = Vec::new();
        compose_diffs(
            &mut self.grid,
            &mut self.subs[i].0,
            &mut self.rows,
            &mut out,
        )
        .expect("compose");
        out
    }

    fn step(&mut self, step: &MirrorStep) {
        match step {
            MirrorStep::Feed(bytes) => self.parser.advance(&mut self.grid, bytes),
            MirrorStep::Drain => self.drain(),
            MirrorStep::Cycle(which) => {
                self.fan_out();
                self.rows.begin_cycle(self.subs.len());
                for (i, &compose) in which.iter().enumerate() {
                    if compose {
                        let out = self.compose(i);
                        Self::apply(&mut self.subs[i].1, &out);
                    }
                }
            }
            MirrorStep::Resize(rows, cols) => {
                if self.grid.on_alternate_screen() {
                    self.grid.resize(*rows, *cols);
                } else {
                    drop(self.grid.reflow(*rows, *cols));
                }
                for (stream, _) in &mut self.subs {
                    stream.retire_scrolls(*rows);
                }
                self.fan_out();
                self.rows.begin_cycle(self.subs.len());
                for i in 0..self.subs.len() {
                    let out = self.compose(i);
                    let shadow = &mut self.subs[i].1;
                    shadow
                        .apply(&GridMsg::Size {
                            dims: GridDims {
                                rows: *rows,
                                cols: *cols,
                                pixel_w: 0,
                                pixel_h: 0,
                            },
                        })
                        .expect("the mirror admits the resize");
                    Self::apply(shadow, &out);
                }
            }
            MirrorStep::Browse(i, lines) => {
                let viewport = self.grid.clamp_viewport(*lines);
                let stream = &mut self.subs[*i].0;
                if viewport != stream.diff.viewport {
                    stream.diff.viewport = viewport;
                    stream.damage.mark_all();
                }
            }
        }
    }

    /// What a mirror shows besides its cells: the cursor and each row's
    /// soft-wrap bit.
    fn overlay_of(screen: &felis_grid::ScreenBuffer) -> ((u16, u16, bool), Vec<bool>) {
        let cursor = screen.cursor();
        let wraps = (0..screen.rows())
            .map(|r| screen.row_soft_wrap_continued(r))
            .collect();
        ((cursor.row, cursor.col, cursor.visible), wraps)
    }

    /// Every visible cell as a mirror can compare it: the grapheme's
    /// text, the resolved attributes (style ids are per-side), the link.
    fn cells_of(
        screen: &felis_grid::ScreenBuffer,
    ) -> Vec<Vec<(String, felis_grid::Attributes, Option<std::num::NonZeroU16>)>> {
        (0..screen.rows())
            .map(|r| {
                (0..screen.cols())
                    .map(|c| {
                        let cell = screen.cell(r, c).copied().unwrap_or_default();
                        let mut text = String::new();
                        felis_grid::push_cell_text(&cell, screen.cluster_table(), &mut text);
                        (text, *screen.style(cell.style), cell.link)
                    })
                    .collect()
            })
            .collect()
    }
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(512))]

    /// Whatever the parse, the drain cadence, the resizes and each
    /// subscriber's pull cadence, a mirror that composes at the live
    /// view holds exactly the grid's cells afterwards.
    #[test]
    fn every_mirror_ends_each_compose_holding_the_grids_cells(
        rows in 2u16..=8,
        cols in 2u16..=10,
        steps in proptest::collection::vec(mirror_step(), 1..40),
    ) {
        let mut rig = MirrorRig::new(rows, cols);
        for (n, step) in steps.iter().enumerate() {
            rig.step(step);
            if matches!(step, MirrorStep::Cycle(_) | MirrorStep::Resize(..)) {
                let composed: Vec<bool> = match step {
                    MirrorStep::Cycle(which) => which.to_vec(),
                    _ => vec![true; rig.subs.len()],
                };
                let want = MirrorRig::cells_of(rig.grid.screen());
                let want_overlay = MirrorRig::overlay_of(rig.grid.screen());
                for (i, (stream, shadow)) in rig.subs.iter().enumerate() {
                    if composed[i] && stream.diff.viewport == 0 {
                        proptest::prop_assert_eq!(
                            MirrorRig::cells_of(shadow.screen()),
                            want.clone(),
                            "mirror {} after step {} ({:?})", i, n, step
                        );
                        proptest::prop_assert_eq!(
                            MirrorRig::overlay_of(shadow.screen()),
                            want_overlay.clone(),
                            "mirror {} cursor or soft wraps after step {} ({:?})", i, n, step
                        );
                    }
                }
            }
        }
        rig.step(&MirrorStep::Browse(0, 0));
        rig.step(&MirrorStep::Browse(1, 0));
        rig.step(&MirrorStep::Drain);
        rig.step(&MirrorStep::Cycle([true, true]));
        let want = MirrorRig::cells_of(rig.grid.screen());
        let want_overlay = MirrorRig::overlay_of(rig.grid.screen());
        for (i, (_, shadow)) in rig.subs.iter().enumerate() {
            proptest::prop_assert_eq!(
                MirrorRig::cells_of(shadow.screen()),
                want.clone(),
                "mirror {} after the final cycle", i
            );
            proptest::prop_assert_eq!(
                MirrorRig::overlay_of(shadow.screen()),
                want_overlay.clone(),
                "mirror {} cursor or soft wraps after the final cycle", i
            );
        }
    }
}

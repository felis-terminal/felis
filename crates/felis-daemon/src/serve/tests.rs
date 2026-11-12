// `allow`, not `expect`: the same-arm matches live in the Unix-gated
// tests, so the lint does not fire on Windows and an `expect` would be
// unfulfilled there.
#![allow(clippy::match_same_arms)]

use std::time::Duration;

use felis_protocol::messages::{
    AttentionSource, ModifyOtherKeys, MouseProtocol, PaletteAction, ThemeAction,
};

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
use std::num::{NonZeroU32, NonZeroU64};
#[cfg(unix)]
use std::time::Instant;
#[cfg(unix)]
use tempfile::TempDir;

fn hex(id: u128) -> String {
    felis_protocol::SessionHex(id).to_string()
}

mod compose;
#[cfg(unix)]
mod connection_caps;
#[cfg(unix)]
mod create;
mod daemon_stop;
mod handshake;
#[cfg(unix)]
mod images;
mod input;
mod lifecycle;
mod mirror_props;
#[cfg(unix)]
mod mirrors;
mod mode_gate;
#[cfg(unix)]
mod observer_events;
#[cfg(unix)]
mod perf_probes;
#[cfg(unix)]
mod resolution;
mod spawn_args;
#[cfg(unix)]
mod status;
#[cfg(unix)]
mod unattached;
#[cfg(unix)]
mod wire;

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
use crate::test_support::{fixture_path, sh};

use felis_pty::Command;

#[cfg(unix)]
fn shell_factory(body: impl Into<String>) -> SessionFactory {
    let body = body.into();
    Arc::new(move |_| sh(&body, fixture_path()))
}

/// A FIFO a child script blocks on with `read _ < gate` until the test
/// calls [`open_gate`]: a stall that ends when the test's precondition
/// holds rather than after a guessed number of seconds.
#[cfg(unix)]
fn gate(dir: &Path) -> PathBuf {
    let path = dir.join("gate");
    let status = std::process::Command::new("mkfifo")
        .arg(&path)
        .env("PATH", fixture_path())
        .status()
        .expect("run mkfifo");
    assert!(status.success(), "mkfifo {}", path.display());
    path
}

#[cfg(unix)]
async fn open_gate(gate: &Path) {
    let gate = gate.to_owned();
    tokio::time::timeout(
        Duration::from_secs(30),
        tokio::task::spawn_blocking(move || std::fs::write(gate, b"\n")),
    )
    .await
    .expect("the child reaches its gate")
    .expect("the gate writer does not panic")
    .expect("write the gate");
}

#[cfg(unix)]
fn owned_session(body: &str) -> crate::pool::Session {
    let cmd = sh(body, fixture_path());
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

fn ops_stream_into(grid: &Grid, out: &mut Vec<OutEvent>) -> SubscriberStream {
    let images = felis_grid::images::ImageStore::new(1024);
    let placements = felis_grid::images::Placements::new();
    SubscriberStream::rehydrated(ConnectionMode::Ops, grid, &images, &placements, out)
        .expect("rehydrate compose")
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
            image_regions: false,
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
            image_regions: false,
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

#[cfg(unix)]
async fn connect(
    path: &Path,
) -> io::Result<(
    felis_transport::local::ReadHalf,
    felis_transport::local::WriteHalf,
)> {
    felis_transport::local::connect(path).await
}

#[cfg(unix)]
fn spawn_daemon(path: &Path, pool: Arc<Mutex<SessionPool>>, factory: SessionFactory) {
    spawn_daemon_with_caps(path, pool, factory, DaemonCaps::default());
}

#[cfg(unix)]
fn spawn_daemon_with_caps(
    path: &Path,
    pool: Arc<Mutex<SessionPool>>,
    factory: SessionFactory,
    caps: DaemonCaps,
) {
    let serving = bind_unix(path, caps, pool, factory).expect("the test daemon binds its socket");
    tokio::spawn(async move { drop(serving.await) });
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
#[track_caller]
fn assert_mode_denied<T: std::fmt::Debug>(
    result: &Result<T, ConnError>,
    mode: ConnectionMode,
    attempt: &str,
) {
    assert!(
        matches!(result, Err(ConnError::ModeDenied { mode: denied, .. }) if *denied == mode),
        "{attempt} must end the {mode:?} connection, got {result:?}",
    );
}

#[cfg(unix)]
type Halves = (
    FrameReader<felis_transport::local::ReadHalf>,
    FrameWriter<felis_transport::local::WriteHalf>,
);

/// A connection to the daemon at `path`, past the handshake as `mode`.
#[cfg(unix)]
async fn dial(path: &Path, mode: ConnectionMode, pull_paced: bool) -> Halves {
    let (read_half, write_half) = connect(path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome_as(&mut reader, &mut writer, mode, pull_paced).await;
    (reader, writer)
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
            image_regions: false,
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
            image_regions: false,
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

/// On Windows a name is UTF-16LE code units, so `b"SHELL"` is an odd-length
/// string `sanitize_base` refuses; every fixture goes through this.
fn entry(name: &str, value: &str) -> crate::child_env::EnvEntry {
    (
        felis_pty::env_bytes(OsStr::new(name)),
        felis_pty::env_bytes(OsStr::new(value)),
    )
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

/// `spawn_daemon_with_caps` with the serve future kept, for the stop
/// tests: the daemon exiting is the thing under test.
#[cfg(unix)]
fn spawn_daemon_watching(
    path: &Path,
    pool: Arc<Mutex<SessionPool>>,
    factory: SessionFactory,
    caps: DaemonCaps,
) -> tokio::task::JoinHandle<Result<(), ServeError>> {
    let serving = bind_unix(path, caps, pool, factory).expect("the test daemon binds its socket");
    tokio::spawn(serving)
}

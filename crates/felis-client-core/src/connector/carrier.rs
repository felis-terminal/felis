use std::path::PathBuf;
use std::time::{Duration, Instant};

use felis_protocol::{
    ConnectionMode,
    messages::{ConnToClientMsg, ConnToDaemonMsg},
    preface::{self, ClientPreface, DaemonPreface, NegotiationError},
};
use felis_transport::{
    ConnectionDriver, Endpoint, FrameReader, FrameWriter, Incoming,
    local::{self, ReadHalf, WriteHalf},
    preface::{read_daemon_preface, write_client_preface},
    retry::{RetryPolicy, retry_while},
};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::{ChildStdin, ChildStdout, Command};

use super::{ConnectError, Connection, check_mode_minor};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Offer {
    pub mode: ConnectionMode,
    pub pull_paced: bool,
}

impl Offer {
    /// The local carrier pulls per vsync; the SSH carrier eager-pushes,
    /// a round trip per frame being a poor fit for its latency
    /// (docs/explanation/rendering/pipeline.md "Demand-driven emission").
    #[must_use]
    pub const fn window(pull_paced: bool) -> Self {
        Self {
            mode: ConnectionMode::Window,
            pull_paced,
        }
    }

    #[must_use]
    pub const fn ops() -> Self {
        Self {
            mode: ConnectionMode::Ops,
            pull_paced: false,
        }
    }

    #[must_use]
    pub const fn observer() -> Self {
        Self {
            mode: ConnectionMode::Observer,
            pull_paced: false,
        }
    }
}

pub async fn connect(
    endpoint: impl Into<Endpoint>,
    offer: Offer,
) -> Result<Connection<ReadHalf, WriteHalf>, ConnectError> {
    let (read_half, write_half) = local::connect(endpoint)
        .await
        .map_err(ConnectError::Connect)?;
    handshake_over(read_half, write_half, offer).await
}

pub(crate) async fn connect_carrier_with_retry(
    carrier: Carrier,
    offer: Offer,
    policy: RetryPolicy,
) -> Result<CarrierConnection, ConnectError> {
    retry_while(
        || connect_carrier(carrier.clone(), offer, RemoteSpawn::Allow),
        policy,
        ConnectError::is_transient,
    )
    .await
    .map_err(|err| err.source)
}

/// `ssh_args` are spliced verbatim as argv entries, never through a shell:
/// which of them take values is `ssh`'s grammar, not felis's (principle 4)
/// (docs/reference/ipc.md "Cross-host carrier: SSH stdio").
#[must_use]
pub(super) fn relay_command(host: &str, ssh_args: &[String], remote_spawn: RemoteSpawn) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.args(ssh_args)
        .arg(host)
        .arg("felis-daemon")
        .arg("relay");
    if remote_spawn == RemoteSpawn::Refuse {
        cmd.arg("--no-spawn");
    }
    cmd
}

/// `cmd.stderr` stays at the parent default so SSH password prompts
/// and daemon panic messages reach the user's terminal
/// (docs/reference/ipc.md "Cross-host carrier: SSH stdio").
pub(super) fn open_stdio_command(
    cmd: Command,
) -> Result<(CarrierReader, CarrierWriter), ConnectError> {
    let session = felis_transport::spawn_command(cmd)?;
    // The Child handle is dropped here; `ssh` survives until the
    // caller's writer half closes its stdin.
    let felis_transport::StdioSession { reader, writer, .. } = session;
    Ok((
        CarrierReader::Ssh(reader.into_inner()),
        CarrierWriter::Ssh(writer.into_inner()),
    ))
}

/// The preface runs before a [`FrameReader`] exists: the reader's
/// read-ahead would absorb the daemon's reply into a buffer the preface
/// decoder cannot see.
pub(super) async fn handshake_over<R, W>(
    mut read_half: R,
    mut write_half: W,
    offer: Offer,
) -> Result<Connection<R, W>, ConnectError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let offered = ClientPreface::CURRENT;
    write_client_preface(&mut write_half, offered).await?;
    let reply = read_daemon_preface(&mut read_half).await?;
    let daemon_advertised = match reply {
        DaemonPreface::Accept { major, minor } => (major, minor),
        _ => (offered.major, offered.minor),
    };
    let effective_minor = match preface::confirm_accept(offered, reply) {
        Ok(minor) => minor,
        Err(NegotiationError::AcceptedUnofferedMajor { offered, accepted }) => {
            return Err(ConnectError::AcceptedUnofferedMajor { offered, accepted });
        }
        Err(NegotiationError::Refused { min, max }) => {
            return Err(ConnectError::MajorMismatch {
                client_major: offered.major,
                daemon_min: min,
                daemon_max: max,
            });
        }
        Err(NegotiationError::Unknown { status, words }) => {
            return Err(ConnectError::UnknownPrefaceStatus {
                status,
                words,
                client_major: offered.major,
            });
        }
    };

    let mut reader = FrameReader::new(read_half);
    let mut writer = FrameWriter::new(write_half, effective_minor);
    // Built before the `Welcome` is read, so the handshake frame is
    // judged by the same state machine as every later frame.
    let mut driver = ConnectionDriver::client(offer.mode);
    driver.preface_done();

    check_mode_minor(offer.mode.since_minor(), effective_minor)?;
    writer
        .send(&ConnToDaemonMsg::Hello {
            mode: offer.mode,
            pull_paced: offer.pull_paced,
        })
        .await?;

    let reply = reader
        .next_frame()
        .await?
        .ok_or(ConnectError::EofBeforeWelcome)?;
    let Incoming::Control(welcome) = driver.classify(&reply)? else {
        unreachable!("the handshake phase admits only Conn frames");
    };
    // Surfaced before the close that follows it, which the retry loop
    // would otherwise read as a transient EOF.
    if let ConnToClientMsg::Refused { reason, detail } = welcome {
        return Err(ConnectError::Refused { reason, detail });
    }
    let ConnToClientMsg::Welcome { identity } = welcome else {
        return Err(ConnectError::NotWelcome);
    };
    driver.handshake_done(offer.mode);

    Ok(Connection {
        reader,
        writer,
        driver,
        pending: std::collections::VecDeque::new(),
        effective_minor,
        accepted_major: daemon_advertised.0,
        daemon_minor: daemon_advertised.1,
        daemon_identity: identity,
        abandoned: false,
    })
}

/// An enum rather than `Box<dyn AsyncRead>`: no per-frame vtable hop,
/// and the carrier set is closed at two (TCP / mosh are rejected,
/// `docs/explanation/architecture/ipc.md`).
pub enum CarrierReader {
    Local(ReadHalf),
    Ssh(ChildStdout),
}

pub enum CarrierWriter {
    Local(WriteHalf),
    Ssh(ChildStdin),
}

// Every underlying half is `Unpin`, so no `pin-project` is needed.
impl AsyncRead for CarrierReader {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Local(r) => std::pin::Pin::new(r).poll_read(cx, buf),
            Self::Ssh(r) => std::pin::Pin::new(r).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for CarrierWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Local(w) => std::pin::Pin::new(w).poll_write(cx, buf),
            Self::Ssh(w) => std::pin::Pin::new(w).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Local(w) => std::pin::Pin::new(w).poll_flush(cx),
            Self::Ssh(w) => std::pin::Pin::new(w).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Local(w) => std::pin::Pin::new(w).poll_shutdown(cx),
            Self::Ssh(w) => std::pin::Pin::new(w).poll_shutdown(cx),
        }
    }
}

pub type CarrierConnection = Connection<CarrierReader, CarrierWriter>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Carrier {
    Local(Endpoint),
    Ssh {
        destination: String,
        ssh_args: Vec<String>,
    },
}

impl Carrier {
    #[must_use]
    pub fn local_socket(&self) -> Option<PathBuf> {
        match self {
            #[cfg(unix)]
            Self::Local(endpoint) => Some(endpoint.path().to_path_buf()),
            // The pipe name round-trips through `PathBuf` as the same
            // string (`Endpoint`'s `From<PathBuf>`), never a filesystem op.
            #[cfg(windows)]
            Self::Local(endpoint) => Some(PathBuf::from(endpoint.pipe_name())),
            Self::Ssh { .. } => None,
        }
    }
}

/// Over SSH the spawn decision lives in `felis-daemon relay`, so the
/// dial has to carry it; the local carrier ignores it
/// ([`crate::connect_or_spawn_daemon`] is the local step).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteSpawn {
    Allow,
    /// Headless verbs: a cold remote socket surfaces as "no daemon"
    /// (`felis-daemon relay --no-spawn`), as the local verbs hold.
    Refuse,
}

/// The carrier erasure happens on the raw byte halves at connect time:
/// unwrapping a post-attach `FrameReader` would drop rehydrate bytes
/// already read into its buffer.
pub async fn connect_carrier(
    carrier: Carrier,
    offer: Offer,
    remote_spawn: RemoteSpawn,
) -> Result<CarrierConnection, ConnectError> {
    let (read_half, write_half) = open_carrier(carrier, remote_spawn).await?;
    handshake_over(read_half, write_half, offer).await
}

async fn open_carrier(
    carrier: Carrier,
    remote_spawn: RemoteSpawn,
) -> Result<(CarrierReader, CarrierWriter), ConnectError> {
    match carrier {
        Carrier::Local(endpoint) => {
            let (read_half, write_half) = local::connect(endpoint)
                .await
                .map_err(ConnectError::Connect)?;
            Ok((
                CarrierReader::Local(read_half),
                CarrierWriter::Local(write_half),
            ))
        }
        Carrier::Ssh {
            destination,
            ssh_args,
        } => open_stdio_command(relay_command(&destination, &ssh_args, remote_spawn)),
    }
}

/// Why a bounded dial ended, with the phase an expiry caught. A caller
/// diagnosing an endpoint needs them apart: a deadline that expired
/// after the connect means something is listening there, which one that
/// expired during the connect does not.
#[derive(Debug, Error)]
pub enum BoundedDialError {
    #[error("the connect did not complete within {}s", deadline.as_secs())]
    ConnectTimedOut { deadline: Duration },
    #[error(
        "it accepted the connection but did not finish the felis handshake within {}s",
        deadline.as_secs()
    )]
    HandshakeTimedOut { deadline: Duration },
    #[error(transparent)]
    Connect(#[from] ConnectError),
}

/// One dial under one overall deadline, which covers the connect and
/// the handshake together and names the phase it caught.
///
/// # Errors
/// [`BoundedDialError`] on an expiry or any [`ConnectError`].
pub async fn dial_bounded(
    carrier: Carrier,
    offer: Offer,
    remote_spawn: RemoteSpawn,
    deadline: Duration,
) -> Result<CarrierConnection, BoundedDialError> {
    let started = Instant::now();
    let halves = tokio::time::timeout(deadline, open_carrier(carrier, remote_spawn))
        .await
        .map_err(|_elapsed| BoundedDialError::ConnectTimedOut { deadline })??;
    let remaining = deadline.saturating_sub(started.elapsed());
    let (read_half, write_half) = halves;
    let conn = tokio::time::timeout(remaining, handshake_over(read_half, write_half, offer))
        .await
        .map_err(|_elapsed| BoundedDialError::HandshakeTimedOut { deadline })??;
    Ok(conn)
}

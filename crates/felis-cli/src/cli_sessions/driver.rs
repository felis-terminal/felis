use std::ops::ControlFlow;

use anyhow::Result;
use felis_client_core::{ConnectError, Connection};
use felis_protocol::{
    codec,
    messages::{ConnToClientMsg, Directed, StreamErrorReason, StreamId, Subject},
};
use felis_transport::{Delivery, Incoming};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::cli_output::{ErrorKind, Reporter};
use crate::conn::RehydrateError;

pub(super) enum DrainTick {
    /// The per-attempt read timed out; the wall-clock deadline has not
    /// elapsed.
    TimedOut,
    DeadlineExceeded,
    Eof,
    /// The read failed below the driver.
    Error(ConnectError),
    /// The driver refused a frame. Fatal: the peer has already lost
    /// the stream position that would make the next frame meaningful,
    /// so no caller may answer this with [`Stall::Retry`].
    Corrupt(felis_transport::DriverError),
}

pub(super) enum Stall {
    Retry,
    /// Stop with no error; [`drain_frames`] returns `Ok(false)`.
    Stop,
    /// Stop with a hard failure; the caller propagates the exit code
    /// without running its bookend (`Detach`), since the connection is
    /// presumed broken.
    Fail(i32),
}

/// The terminal is a `Conn` frame, not a member of the family being
/// drained, which is why it cannot arrive as an `M`.
pub(super) enum Drained<M> {
    Item(M),
    End {
        /// The daemon's own tally.
        count: u32,
    },
    Failed {
        reason: StreamErrorReason,
        detail: String,
    },
}

/// Drains frames until `stream` terminates or `on_frame`/`on_stall` halts.
/// Refused frames abort because boundaries desynced; other families pass.
/// Matching `stream` terminals avoid confusing concurrent operations.
/// Returns `Ok(true)` if stopped by `on_frame`, `Ok(false)` on `Stall::Stop`.
pub(super) async fn drain_frames<R, W, M>(
    conn: &mut Connection<R, W>,
    stream: Option<StreamId>,
    deadline: Option<tokio::time::Instant>,
    per_attempt: std::time::Duration,
    mut on_stall: impl FnMut(DrainTick) -> Stall,
    mut on_frame: impl FnMut(Drained<M>) -> ControlFlow<()>,
) -> Result<bool, i32>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    M: codec::WireCodec + Directed,
{
    loop {
        if deadline.is_some_and(|d| tokio::time::Instant::now() >= d) {
            return match on_stall(DrainTick::DeadlineExceeded) {
                Stall::Fail(code) => Err(code),
                Stall::Retry | Stall::Stop => Ok(false),
            };
        }
        // Through the connection, not the raw reader: frames a verb
        // parked while its own reply was outstanding are queued there.
        let frame_res = tokio::time::timeout(per_attempt, conn.next_frame()).await;
        let frame = match frame_res {
            Ok(Ok(Some(f))) => f,
            Ok(Ok(None)) => match on_stall(DrainTick::Eof) {
                Stall::Fail(code) => return Err(code),
                Stall::Stop => return Ok(false),
                Stall::Retry => continue,
            },
            Ok(Err(e)) => match on_stall(DrainTick::Error(e)) {
                Stall::Fail(code) => return Err(code),
                Stall::Stop => return Ok(false),
                Stall::Retry => continue,
            },
            Err(_) => match on_stall(DrainTick::TimedOut) {
                Stall::Fail(code) => return Err(code),
                Stall::Stop => return Ok(false),
                Stall::Retry => continue,
            },
        };
        let incoming = match conn.driver.classify(&frame) {
            Ok(incoming) => incoming,
            // A retry would re-read past a frame that already broke the
            // contract, so every answer but `Fail` collapses to a stop.
            Err(err) => match on_stall(DrainTick::Corrupt(err)) {
                Stall::Fail(code) => return Err(code),
                Stall::Retry | Stall::Stop => return Ok(false),
            },
        };
        // The guarded payload arm must lead: the ignore arm is only
        // reachable for the families this drain does not read.
        let drained = match incoming {
            Incoming::Payload(payload) if payload.kind == M::KIND => {
                match conn.driver.decode::<M>(&payload) {
                    Ok(Delivery::Deliver(delivered)) => Drained::Item(delivered.msg),
                    // Neither reaches a client: only the daemon refuses
                    // an opening request, and this drain cancels nothing.
                    Ok(Delivery::DroppedAfterCancel | Delivery::RefuseStream { .. }) => continue,
                    Err(err) => match on_stall(DrainTick::Corrupt(err)) {
                        Stall::Fail(code) => return Err(code),
                        Stall::Retry | Stall::Stop => return Ok(false),
                    },
                }
            }
            Incoming::Control(ConnToClientMsg::End { stream_id, count })
                if Some(stream_id) == stream =>
            {
                Drained::End { count }
            }
            Incoming::Control(ConnToClientMsg::Error {
                subject: Subject::Stream(stream_id),
                reason,
                detail,
            }) if Some(stream_id) == stream => Drained::Failed { reason, detail },
            // A cancel racing its own terminal, or another operation's
            // terminal.
            Incoming::Control(_) | Incoming::CancelIgnored { .. } => continue,
            // Another family's frame on a shared connection. Drained
            // through the driver rather than around it: this verb has
            // no use for the message, but the arm's row still holds
            // and only the decode enforces it (REQ-113a).
            Incoming::Payload(other) => {
                if let Err(err) = conn.driver.admit_drained(&other) {
                    match on_stall(DrainTick::Corrupt(err)) {
                        Stall::Fail(code) => return Err(code),
                        Stall::Retry | Stall::Stop => return Ok(false),
                    }
                }
                continue;
            }
        };
        if on_frame(drained) == ControlFlow::Break(()) {
            return Ok(true);
        }
    }
}

/// Discards the rehydrate burst: subscribers must consume outboxes before
/// requesting, but capture reads regions directly. A missing `RehydrateEnd`
/// exits 2 to prevent partial captures (docs/reference/cli.md streaming-read contract).
pub(super) async fn drain_rehydrate<R, W>(
    conn: &mut Connection<R, W>,
    out: &Reporter,
) -> Result<(), i32>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    crate::conn::drain_rehydrate(conn)
        .await
        .map_err(|err| match err {
            RehydrateError::Closed => out.fail(
                ErrorKind::DaemonLost,
                "capture: daemon closed the connection mid-sync",
            ),
            RehydrateError::TimedOut => out.fail(
                ErrorKind::DaemonLost,
                "capture: the daemon did not finish syncing the screen in time",
            ),
            RehydrateError::Read(e) => out.fail(
                ErrorKind::DaemonLost,
                format!("capture: connection error: {e}"),
            ),
            RehydrateError::Corrupt(err) => out.fail(
                ErrorKind::Protocol,
                format!("capture: daemon sent an unreadable frame: {err}"),
            ),
        })
}

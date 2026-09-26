//! `felis bridge`: persistent JSONL stdio bridge for non-Rust clients.
//!
//! Exposes an LSP-shaped JSONL interface over stdin/stdout while speaking
//! protobuf to the daemon (docs/reference/ipc.md).

mod admission;
mod core;
mod envelope;
mod link;
mod ops;
mod stdio;
#[cfg(test)]
mod tests;

#[cfg(all(test, feature = "schema"))]
pub(crate) use admission::{DaemonOp, Operation};

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use felis_client_core::{Offer, Reconnector, RemoteSpawn};
use serde_json::Value;
use tokio::io::BufReader;

use self::core::Core;
use self::envelope::{BridgeError, error_object};
use self::link::{Link, dial};
use self::stdio::{BridgeLine, BridgeLines, Out, OutputClosed};
use crate::cli_output::ErrorKind;

/// How long stream cancellation is given to produce the daemon's own
/// terminals at shutdown before the bridge writes its own: only enough
/// that a terminal already on the wire wins over a synthesized one.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

/// How long already-written requests are given to arrive once the
/// daemon is gone: a request that vanishes is worse than one that
/// fails, but the client's stdin may stay open long after the daemon
/// died.
const LOSS_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_millis(100);

/// Requests whose terminal has not reached stdout yet.
const MAX_IN_FLIGHT: usize = 64;

/// Attached-session and observer links held beside the permanent anchor.
const MAX_AUXILIARY_LINKS: usize = 32;

/// Non-terminal lines that may wait for stdout. Each admitted operation
/// reserves a separate terminal slot in the same queue.
const OUTPUT_ITEM_CAP: usize = 64;

/// Frames parked between one daemon-link pump and its operation task.
const STREAM_BUFFER_CAP: usize = 16;

/// String ids are bounded independently of the request line so the
/// in-flight table cannot retain line-sized keys.
const MAX_ID_BYTES: usize = felis_protocol::messages::MAX_TAG_BYTES;

const MAX_SAFE_JSON_INTEGER: u64 = (1_u64 << 53) - 1;

/// Not `Value::as_u64`, which refuses every number serde parsed into
/// its float variant: JSON Schema's `integer` counts `1.0` as the
/// integer 1, so a request the published schemas accept must be a
/// request this bridge accepts.
fn json_integer(value: &Value) -> Option<u64> {
    let number = value.as_number()?;
    number.as_u64().or_else(|| {
        let float = number.as_f64()?;
        (float.is_finite() && float.fract() == 0.0 && (0.0..=MAX_U64_AS_F64).contains(&float))
            .then_some(float as u64)
    })
}

/// The largest `f64` that converts to a `u64` without saturating.
const MAX_U64_AS_F64: f64 = 18_446_744_073_709_549_568.0;

/// The CLI contract's 2 = daemon-unreachable / protocol error
/// (docs/reference/ipc.md "CLI clients").
const EXIT_DAEMON: i32 = 2;

/// The CLI contract's 1, distinct from the clean `0` of stdin EOF: a
/// supervisor must tell "my client closed the bridge" from "the channel
/// to my client broke".
const EXIT_FAILED: i32 = 1;

/// The bridge picks its [`Offer`] per operation but never its spawn
/// policy: a long-lived multiplexer is still a read/drive client.
pub(crate) const REMOTE_SPAWN: RemoteSpawn = RemoteSpawn::Refuse;

/// The daemon connection is opened before the first line is read, and
/// a cold socket ends the process instead of failing each request in
/// turn, so "the bridge is running" means "the daemon is reachable".
/// The bridge never starts a daemon: a supervised helper must not
/// conjure daemons as a side effect of being launched.
pub(crate) fn run(runtime: &tokio::runtime::Runtime, target: &Reconnector) -> Result<i32> {
    runtime.block_on(serve(target.clone()))
}

async fn serve(target: Reconnector) -> Result<i32> {
    let (out, writer_task) = Out::start();

    let anchor = match dial(&target, Offer::ops()).await {
        Ok(conn) => Link::start(conn, "ops"),
        Err(err) => {
            let code = if err.kind == ErrorKind::AtCapacity {
                EXIT_FAILED
            } else {
                EXIT_DAEMON
            };
            let _emitted = out.emit(error_object(&Value::Null, &err)).await;
            let _closed = out.close().await;
            let _joined = writer_task.await;
            return Ok(if out.has_failed() { EXIT_FAILED } else { code });
        }
    };

    let core = Arc::new(Core {
        target,
        out: out.clone(),
        anchor,
        sessions: tokio::sync::Mutex::new(HashMap::new()),
        active: std::sync::Mutex::new(HashMap::new()),
        pending_replies: std::sync::Mutex::new(HashMap::new()),
        links: Arc::new(tokio::sync::Semaphore::new(MAX_AUXILIARY_LINKS)),
    });

    let mut lines = BridgeLines::new(BufReader::new(tokio::io::stdin()));
    let reason = loop {
        tokio::select! {
            line = lines.next() => match line {
                Ok(Some(BridgeLine::Text(line))) => {
                    if core.accept(&line).await.is_err() {
                        break Shutdown::OutputFailed(out.failure_detail());
                    }
                }
                Ok(Some(BridgeLine::TooLong(err))) => {
                    if out.emit(error_object(&Value::Null, &BridgeError::over_limit(&err))).await.is_err() {
                        break Shutdown::OutputFailed(out.failure_detail());
                    }
                }
                Ok(None) => break Shutdown::Eof,
                Err(err) => {
                    tracing::error!(%err, "bridge: stdin read failed; shutting down");
                    break Shutdown::InputFailed(err.to_string());
                }
            },
            () = core.anchor.lost() => break Shutdown::DaemonLost,
            detail = out.failed() => break Shutdown::OutputFailed(detail),
        }
    };

    let err = reason.error();
    if matches!(reason, Shutdown::DaemonLost)
        && answer_unread(&core, &mut lines, &err).await.is_err()
    {
        let output = Shutdown::OutputFailed(out.failure_detail());
        let output_err = output.error();
        let code = core.shutdown(&output_err, output.code(), false).await;
        let _closed = out.close().await;
        let _joined = writer_task.await;
        return Ok(if out.has_failed() { EXIT_FAILED } else { code });
    }
    let output_available = !matches!(reason, Shutdown::OutputFailed(_));
    let code = core.shutdown(&err, reason.code(), output_available).await;
    let _closed = out.close().await;
    let _joined = writer_task.await;
    Ok(if out.has_failed() { EXIT_FAILED } else { code })
}

/// Decides the exit code and whether the outstanding work is
/// *canceled* or *failed*.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Shutdown {
    /// Outstanding streams are canceled, not failed: nothing went wrong.
    Eof,
    /// A failure, so a supervisor is not told the client left cleanly.
    InputFailed(String),
    OutputFailed(String),
    DaemonLost,
}

impl Shutdown {
    fn error(&self) -> BridgeError {
        match self {
            Self::Eof => BridgeError::new(ErrorKind::Canceled, "the bridge client closed stdin"),
            Self::InputFailed(detail) => BridgeError::new(
                ErrorKind::InputFailed,
                format!("the bridge's stdin failed: {detail}"),
            ),
            Self::OutputFailed(detail) => BridgeError::new(
                ErrorKind::OutputFailed,
                format!("the bridge's stdout failed: {detail}"),
            ),
            Self::DaemonLost => {
                BridgeError::new(ErrorKind::DaemonLost, "the daemon connection was lost")
            }
        }
    }

    const fn code(&self) -> i32 {
        match self {
            Self::Eof => 0,
            Self::InputFailed(_) | Self::OutputFailed(_) => EXIT_FAILED,
            Self::DaemonLost => EXIT_DAEMON,
        }
    }
}

/// Answer the requests the client had already written when the daemon
/// died. Bounded by [`LOSS_DRAIN_GRACE`] rather than EOF: what is owed
/// is an answer to what was sent, not a wait for a client that may
/// never close its end.
async fn answer_unread(
    core: &Arc<Core>,
    lines: &mut BridgeLines<BufReader<tokio::io::Stdin>>,
    err: &BridgeError,
) -> Result<(), OutputClosed> {
    let deadline = tokio::time::Instant::now() + LOSS_DRAIN_GRACE;
    while let Ok(Ok(Some(line))) = tokio::time::timeout_at(deadline, lines.next()).await {
        let line = match line {
            BridgeLine::Text(line) => line,
            BridgeLine::TooLong(over) => {
                core.answer_immediate(
                    &Value::Null,
                    error_object(&Value::Null, &BridgeError::over_limit(&over)),
                )
                .await?;
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        core.answer_after_loss(&line, err).await?;
    }
    Ok(())
}

/// Every mutex here guards plain bookkeeping with no invariant a
/// panicking holder could leave half-applied; propagating the poison
/// would turn one operation's panic into the bridge refusing every
/// later request.
trait LockOrPoisoned<T> {
    fn lock_or_poisoned(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> LockOrPoisoned<T> for std::sync::Mutex<T> {
    fn lock_or_poisoned(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

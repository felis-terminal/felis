//! The predecessor's half on Windows: after the barrier, the quiesce,
//! and the dump, spawn the successor, duplicate every carried handle
//! into it, and leave once it is ready
//! (`docs/explanation/architecture/overview.md` "In-place upgrade").

use std::{ffi::OsStr, path::PathBuf, sync::Arc, time::Duration};

use felis_pty::handover::{Handover, Process};
use felis_transport::handover::{ChannelServer, open_channel};
use tokio::{net::windows::named_pipe::NamedPipeServer, sync::Mutex};
use tracing::{info, warn};

use super::{
    Refusal, UpgradeState, channel,
    dump::{ConptyDump, Dump},
    orchestrate::{Undo, begin, carry_console, check_replaceable, quiesce_and_dump},
};
use crate::SessionPool;

/// From the spawn to the successor's `ready`: its start, the connect,
/// the duplication, the dump, and its restore up to the commit.
const HANDOVER_TIMEOUT: Duration = Duration::from_secs(15);

/// The commit is a few bytes on a pipe the successor holds open.
const COMMIT_BOUND: Duration = Duration::from_secs(2);

/// A successor that has adopted everything and is waiting for the
/// commit. Until [`commit`](Self::commit) writes it, this process may
/// still end the successor and resume.
pub struct Ready {
    successor: Process,
    channel: NamedPipeServer,
    undo: Undo,
}

/// Runs every step up to the successor's `ready`. A refusal has already
/// ended the successor and undone the rest.
pub async fn prepare(
    successor: PathBuf,
    pool: &Arc<Mutex<SessionPool>>,
    state: &Arc<UpgradeState>,
) -> Result<Ready, Refusal> {
    check_replaceable(state)?;
    felis_pty::handover::available().map_err(Refusal::Unsupported)?;
    let Some(pipe_name) = state.listener() else {
        return Err(Refusal::Unsupported(
            "this daemon serves no pipe to hand over".to_owned(),
        ));
    };
    let mut undo = begin(&successor, pool, state).await?;
    let (dump, carried) = match quiesce_and_dump(pool, 0, &mut undo, carry_console).await {
        Ok(dumped) => dumped,
        Err(refusal) => {
            undo.run().await;
            return Err(refusal);
        }
    };
    let server = match open_channel() {
        Ok(server) => server,
        Err(err) => {
            undo.run().await;
            return Err(Refusal::ProbeFailed(format!(
                "open the upgrade channel: {err}"
            )));
        }
    };
    let args = [
        OsStr::new("serve"),
        OsStr::new("--socket"),
        OsStr::new(&pipe_name),
        OsStr::new("--resume"),
        OsStr::new(server.name()),
    ];
    let spawned = match Process::spawn_detached(&successor, &args) {
        Ok(spawned) => spawned,
        Err(err) => {
            undo.run().await;
            return Err(Refusal::ProbeFailed(format!(
                "run {}: {err}",
                successor.display()
            )));
        }
    };
    info!(
        pid = spawned.pid(),
        "upgrade: handing over to the successor"
    );
    let handed = tokio::time::timeout(
        HANDOVER_TIMEOUT,
        hand_over(server, &spawned, state, dump, &carried),
    )
    .await
    .unwrap_or_else(|_elapsed| {
        Err(Refusal::Timeout(format!(
            "{} did not take over in time",
            successor.display()
        )))
    });
    match handed {
        Ok(channel) => Ok(Ready {
            successor: spawned,
            channel,
            undo,
        }),
        Err(refusal) => {
            end(spawned).await;
            undo.run().await;
            Err(refusal)
        }
    }
}

async fn hand_over(
    server: ChannelServer,
    successor: &Process,
    state: &UpgradeState,
    mut dump: Dump,
    carried: &[Handover],
) -> Result<NamedPipeServer, Refusal> {
    let failed = |what: &str, err: std::io::Error| Refusal::ProbeFailed(format!("{what}: {err}"));
    let mut channel = server
        .accept(successor.pid())
        .await
        .map_err(|err| failed("connect the successor", err))?;
    dump.predecessor = Process::current_into(successor)
        .map_err(|err| failed("hand the successor this process", err))?;
    dump.creator = state
        .creator_into(successor)
        .map_err(|err| failed("hand the successor the pipe's creator", err))?;
    for (session, handover) in dump.sessions.iter_mut().zip(carried) {
        let values = handover
            .values_in(successor)
            .map_err(|err| failed("hand over a session's console", err))?;
        session.conpty = Some(ConptyDump {
            signal: values.signal,
            reference: values.reference,
            host_process: values.host_process,
            output: values.output,
            input: values.input,
            child: values.child,
        });
    }
    channel::send(&mut channel, channel::DUMP, &dump.encode())
        .await
        .map_err(|err| failed("send the dump", err))?;
    match channel::recv(&mut channel).await {
        Ok((channel::READY, _)) => Ok(channel),
        Ok((channel::FAILED, said)) => Err(Refusal::ProbeFailed(
            String::from_utf8_lossy(&said).into_owned(),
        )),
        Ok((tag, _)) => Err(Refusal::ProbeFailed(format!(
            "the successor answered with upgrade frame {tag}"
        ))),
        Err(err) => Err(failed("the successor ended before it was ready", err)),
    }
}

/// Ends a successor that has not been told to commit and waits until it
/// is gone. Nothing it holds outlives it but duplicates, so the parked
/// sessions resume as they were.
async fn end(successor: Process) {
    let ended = tokio::task::spawn_blocking(move || {
        if let Err(err) = successor.terminate() {
            warn!(?err, "upgrade: ending the successor failed");
        }
        successor.wait(None)
    })
    .await;
    if !matches!(ended, Ok(Ok(true))) {
        warn!(?ended, "upgrade: waiting for the successor to end failed");
    }
}

impl Ready {
    /// Gives up before the commit, resuming every session.
    pub async fn abandon(self) {
        end(self.successor).await;
        self.undo.run().await;
    }

    /// Hands the sessions over and exits this process. Returns only when
    /// the successor could not be told, after undoing everything, with
    /// the refusal to report.
    pub async fn commit(mut self) -> Refusal {
        let sent = tokio::time::timeout(
            COMMIT_BOUND,
            channel::send(&mut self.channel, channel::COMMIT, &[]),
        )
        .await;
        if !matches!(sent, Ok(Ok(()))) {
            end(self.successor).await;
            self.undo.run().await;
            return Refusal::ProbeFailed("the successor did not take the commit".to_owned());
        }
        // Gone already, it starts nothing: the sessions are still this
        // process's to resume.
        if matches!(self.successor.wait(Some(Duration::ZERO)), Ok(true)) {
            self.undo.run().await;
            return Refusal::ProbeFailed("the successor exited at the commit".to_owned());
        }
        info!("upgrade: handed over; exiting");
        // No destructor runs: a session's would close its pseudoconsole,
        // which the successor now owns.
        std::process::exit(0)
    }
}

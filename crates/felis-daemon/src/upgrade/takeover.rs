//! The successor's half on Windows: adopt everything the predecessor
//! handed over without starting any of it, report ready, and start only
//! once the predecessor has committed and is gone
//! (`docs/explanation/architecture/overview.md` "In-place upgrade").

use std::{io, path::Path, sync::Arc, time::Duration};

use felis_pty::handover::{Adopted, HandoverValues, Process, prepare_adopt};
use felis_transport::{Endpoint, Listener, handover::connect_channel, local::BindError};
use tokio::{net::windows::named_pipe::NamedPipeClient, sync::Mutex};
use tracing::{info, warn};

use super::{
    UpgradeState, channel,
    dump::{Dump, DumpError, SessionDump},
    restore::register_with,
};
use crate::{SessionPool, SpawnedPty, serve::IdlePolicy};

/// How long a committed predecessor has to exit before the successor
/// ends it. Only the successor may end it from the commit on.
const EXIT_BOUND: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum TakeoverError {
    #[error("connect the upgrade channel: {0}")]
    Channel(io::Error),
    #[error(transparent)]
    Dump(#[from] DumpError),
    #[error("{0}")]
    Adopt(String),
    #[error("listen: {0}")]
    Listen(BindError),
    #[error("the predecessor did not commit: {0}")]
    NoCommit(io::Error),
}

/// Takes over from the predecessor that spawned this process, returning
/// the listener to serve. Every error before the commit leaves the
/// sessions as the predecessor parked them, for it to resume.
pub async fn take_over(
    channel_name: &str,
    socket: &Path,
    pool: &Arc<Mutex<SessionPool>>,
    policy: IdlePolicy,
    state: &UpgradeState,
) -> Result<Listener, TakeoverError> {
    let (mut channel, server_pid) =
        connect_channel(channel_name).map_err(TakeoverError::Channel)?;
    let (dump, predecessor, creator, sessions, listener) =
        match prepare(&mut channel, server_pid, socket).await {
            Ok(prepared) => prepared,
            Err(err) => {
                let said = err.to_string();
                let _told = channel::send(&mut channel, channel::FAILED, said.as_bytes()).await;
                return Err(err);
            }
        };
    channel::send(&mut channel, channel::READY, &[])
        .await
        .map_err(TakeoverError::NoCommit)?;
    channel::expect(&mut channel, channel::COMMIT)
        .await
        .map_err(TakeoverError::NoCommit)?;
    state.set_creator(creator);
    outlast(predecessor).await;
    info!("upgrade: the predecessor is gone; taking over");
    if !crate::graphics::resume_anim_clock(dump.anim_clock_ms) {
        warn!("upgrade: image animations restart their frame clocks");
    }
    pool.lock()
        .await
        .resume_counters(dump.next_sequence, dump.next_attachment_id);
    let count = sessions.len();
    for (session, adopted) in dump.sessions.iter().zip(sessions) {
        let child = adopted.child();
        if let Err(err) = register_with(pool, policy, session, |core| {
            SpawnedPty::adopt(adopted, core)
        })
        .await
        {
            warn!(%err, id = %session.id, "upgrade: a carried session could not be restored");
            end_child(child);
        }
    }
    info!(sessions = count, "upgrade: restored the carried sessions");
    Ok(listener)
}

type Prepared = (Dump, Process, Process, Vec<Adopted>, Listener);

/// Everything before `ready`, with no effect a terminated successor
/// would leave behind: nothing reads, writes, resizes, or signals.
async fn prepare(
    channel: &mut NamedPipeClient,
    server_pid: u32,
    socket: &Path,
) -> Result<Prepared, TakeoverError> {
    let bytes = channel::expect(channel, channel::DUMP)
        .await
        .map_err(TakeoverError::Channel)?;
    let dump = Dump::decode(&bytes)?;
    let predecessor = Process::adopt(dump.predecessor)
        .map_err(|err| TakeoverError::Adopt(format!("adopt the predecessor: {err}")))?;
    if predecessor.pid() != server_pid {
        return Err(TakeoverError::Adopt(format!(
            "the channel's server is pid {server_pid}, not the predecessor {}",
            predecessor.pid()
        )));
    }
    let creator = Process::adopt(dump.creator)
        .map_err(|err| TakeoverError::Adopt(format!("adopt the pipe's creator: {err}")))?;
    let sessions = dump
        .sessions
        .iter()
        .map(adopt)
        .collect::<Result<Vec<_>, _>>()?;
    let listener = Listener::join(&Endpoint::from(socket)).map_err(TakeoverError::Listen)?;
    Ok((dump, predecessor, creator, sessions, listener))
}

fn adopt(session: &SessionDump) -> Result<Adopted, TakeoverError> {
    let conpty = session.conpty.unwrap_or_default();
    prepare_adopt(&HandoverValues {
        signal: conpty.signal,
        reference: conpty.reference,
        host_process: conpty.host_process,
        output: conpty.output,
        input: conpty.input,
        child: conpty.child,
    })
    .map_err(|err| TakeoverError::Adopt(format!("session {}: {err}", session.id)))
}

/// Returns once the committed predecessor has exited, ending it if it
/// outlasts [`EXIT_BOUND`]: an end the kernel completes whatever the
/// predecessor is doing.
async fn outlast(predecessor: Process) {
    let waited = tokio::task::spawn_blocking(move || {
        if !predecessor.wait(Some(EXIT_BOUND))? {
            warn!("upgrade: the predecessor outlasted its commit; ending it");
            predecessor.terminate()?;
            predecessor.wait(None)?;
        }
        Ok::<(), io::Error>(())
    })
    .await;
    if !matches!(waited, Ok(Ok(()))) {
        warn!(?waited, "upgrade: waiting for the predecessor failed");
    }
}

/// A session that cannot run after the commit has no daemon left to
/// reach it, so its shell ends with it, as a destroyed one does.
fn end_child(child: io::Result<Process>) {
    match child.and_then(|child| child.terminate()) {
        Ok(()) => {}
        Err(err) => warn!(?err, "upgrade: ending an unrestored session's child failed"),
    }
}

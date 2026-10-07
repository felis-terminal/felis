//! The successor's half: adopt what the exec carried and register the
//! sessions under their own ids.

use std::{
    num::NonZeroU64,
    os::fd::{AsFd as _, OwnedFd, RawFd},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use felis_transport::{Endpoint, Listener, inherit::take_inherited};
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, WaitOptions};
use tokio::sync::Mutex;
use tracing::{info, warn};

use super::{
    carrier,
    dump::{Dump, DumpError, SessionDump, parse_session_id},
};
use crate::{
    SessionId, SessionPool, SpawnedPty,
    pool::Session,
    serve::{
        IdlePolicy,
        session_task::{Restored, spawn_restored},
    },
};

const HANGUP_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    #[error("read the upgrade dump: {0}")]
    Carrier(std::io::Error),
    #[error(transparent)]
    Dump(#[from] DumpError),
    #[error("adopt the listening socket: {0}")]
    Listener(std::io::Error),
}

/// Adopts the listening socket and every carried session. A session
/// that cannot be adopted has its child ended; a failure that loses the
/// socket ends every child, since nothing could reach them again.
pub async fn restore(
    resume_fd: RawFd,
    socket: &Path,
    pool: &Arc<Mutex<SessionPool>>,
    policy: IdlePolicy,
) -> Result<Listener, RestoreError> {
    let carried = take_inherited(resume_fd).map_err(RestoreError::Carrier)?;
    let bytes = carrier::read(carried.as_fd()).map_err(RestoreError::Carrier)?;
    drop(carried);
    let dump = Dump::decode(&bytes)?;
    let mut sessions = Vec::with_capacity(dump.sessions.len());
    for session in dump.sessions {
        match take_inherited(session.master_fd) {
            Ok(master) => sessions.push((session, master)),
            Err(err) => {
                warn!(?err, id = %session.id, "upgrade: a carried PTY master is missing");
                end_children([&session]).await;
            }
        }
    }
    let listener =
        take_inherited(dump.listen_fd).and_then(|fd| Listener::adopt(fd, &Endpoint::unix(socket)));
    let listener = match listener {
        Ok(listener) => listener,
        Err(err) => {
            end_children(sessions.iter().map(|(session, _)| session)).await;
            return Err(RestoreError::Listener(err));
        }
    };
    pool.lock()
        .await
        .resume_counters(dump.next_sequence, dump.next_attachment_id);
    let count = sessions.len();
    for (session, master) in sessions {
        if let Err(err) = register(pool, policy, &session, master).await {
            warn!(%err, id = %session.id, "upgrade: a carried session could not be restored");
            end_children([&session]).await;
        }
    }
    info!(sessions = count, "upgrade: restored the carried sessions");
    Ok(listener)
}

async fn register(
    pool: &Arc<Mutex<SessionPool>>,
    policy: IdlePolicy,
    dump: &SessionDump,
    master: OwnedFd,
) -> Result<(), String> {
    let id = parse_session_id(&dump.id).ok_or("unparsable session id")?;
    let sequence = NonZeroU64::new(dump.sequence).ok_or("zero sequence")?;
    let spawned = SpawnedPty::adopt(
        master,
        dump.child.pid,
        dump.child.exit_status,
        dump.rows,
        dump.cols,
    )
    .map_err(|err| err.to_string())?;
    let session = Session::from_spawned(spawned);
    if !dump.unwritten.is_empty() {
        session
            .writer
            .write_owned(dump.unwritten.clone(), Some(Box::new(())))
            .map_err(|err| format!("requeue unwritten input: {err}"))?;
    }
    spawn_restored(
        pool,
        session,
        policy,
        SessionId(id),
        dump.pixel_w,
        dump.pixel_h,
        dump.tags.clone(),
        Restored {
            sequence,
            title: dump.title.clone(),
            cwd: dump.cwd.clone(),
            exited: dump.exited,
        },
    )
    .await;
    Ok(())
}

/// Hangs up each child's process group, waits out a grace, then kills
/// every group and reaps. No leader is reaped before its group's last
/// signal, so the group id cannot name anyone else yet; a child the
/// predecessor already reaped is never signaled.
async fn end_children<'a>(sessions: impl IntoIterator<Item = &'a SessionDump>) {
    let pids: Vec<Pid> = sessions
        .into_iter()
        .filter(|session| session.child.exit_status.is_none())
        .filter_map(|session| Pid::from_raw(session.child.pid))
        .collect();
    if pids.is_empty() {
        return;
    }
    let ended = tokio::task::spawn_blocking(move || {
        for pid in &pids {
            let _sent = rustix::process::kill_process_group(*pid, Signal::HUP);
        }
        let deadline = Instant::now() + HANGUP_GRACE;
        while !pids.iter().all(|pid| exited(*pid)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        // An exited leader's group may still hold a descendant that
        // ignored the hangup.
        for pid in &pids {
            let _sent = rustix::process::kill_process_group(*pid, Signal::KILL);
        }
        for pid in pids {
            let _reaped = rustix::process::waitpid(Some(pid), WaitOptions::empty());
        }
    });
    if let Err(err) = ended.await {
        warn!(?err, "upgrade: ending orphaned children failed");
    }
}

/// `true` once the child has exited or is not ours to wait for; leaves
/// it unreaped.
fn exited(pid: Pid) -> bool {
    let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
    !matches!(rustix::process::waitid(WaitId::Pid(pid), options), Ok(None))
}

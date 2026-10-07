//! The predecessor's half: barrier, quiesce, dump, probe, exec.

use std::{
    os::fd::{AsFd as _, AsRawFd as _, OwnedFd, RawFd},
    os::unix::process::{CommandExt as _, ExitStatusExt as _},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use felis_pty::{Parked, Quiescer};
use tokio::{
    sync::{Mutex, mpsc, oneshot},
    task::JoinHandle,
};
use tracing::{info, warn};

use super::{
    Refusal, UpgradeState, carrier,
    dump::{ChildDump, DUMP_VERSION, Dump, SessionDump, SessionState, format_session_id},
    probe,
};
use crate::{
    SessionPool,
    pool::{SessionHandle, UpgradeParts},
    serve::session_task::SessionCmd,
};

const BARRIER_TIMEOUT: Duration = Duration::from_secs(5);
const PARK_TIMEOUT: Duration = Duration::from_secs(5);

/// An upgrade ready to exec: the sessions are parked and the successor
/// has read the dump. Dropped without an exec, it resumes the daemon.
pub struct Prepared {
    successor: PathBuf,
    socket: PathBuf,
    /// Duplicates of the listening socket and every master the dump
    /// names, plus the dump's own file. Close-on-exec comes off these
    /// copies only, and only at the exec.
    carried: Vec<OwnedFd>,
    carrier_fd: RawFd,
    undo: Undo,
}

/// Armed from the moment the pool stops admitting: a refusal runs it,
/// and so does a drop, so a cancelled or panicking upgrade still
/// leaves the daemon serving.
struct Undo {
    pool: Arc<Mutex<SessionPool>>,
    state: Arc<UpgradeState>,
    parked: Vec<Quiescer>,
    /// A park still running on the blocking pool: it outlives a
    /// cancelled upgrade and must be resumed once it ends.
    pending: Option<(Quiescer, JoinHandle<std::io::Result<Parked>>)>,
    /// Session tasks holding their drains since the settle.
    held: Vec<mpsc::Sender<SessionCmd>>,
    armed: bool,
}

impl Undo {
    fn new(pool: &Arc<Mutex<SessionPool>>, state: &Arc<UpgradeState>) -> Self {
        Self {
            pool: Arc::clone(pool),
            state: Arc::clone(state),
            parked: Vec::new(),
            pending: None,
            held: Vec::new(),
            armed: true,
        }
    }

    async fn run(mut self) {
        self.resume_sessions();
        self.pool.lock().await.end_upgrade();
        self.state.gate.open();
        self.armed = false;
    }

    fn resume_sessions(&mut self) {
        for quiescer in self.parked.drain(..) {
            if let Err(err) = quiescer.resume() {
                warn!(?err, "upgrade refused: resuming a session failed");
            }
        }
        for cmd in self.held.drain(..) {
            match cmd.try_send(SessionCmd::Resume) {
                Err(mpsc::error::TrySendError::Full(_)) => {
                    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                        drop(runtime.spawn(async move {
                            let _sent = cmd.send(SessionCmd::Resume).await;
                        }));
                    }
                }
                Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
            }
        }
    }
}

impl Drop for Undo {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        warn!("upgrade abandoned midway; resuming");
        self.resume_sessions();
        let pool = Arc::clone(&self.pool);
        let state = Arc::clone(&self.state);
        let pending = self.pending.take();
        let finish = async move {
            if let Some((quiescer, parking)) = pending {
                let _parked = parking.await;
                if let Err(err) = quiescer.resume() {
                    warn!(?err, "upgrade abandoned: resuming a session failed");
                }
            }
            pool.lock().await.end_upgrade();
            state.gate.open();
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            drop(runtime.spawn(finish));
        } else {
            if let Ok(mut pool) = self.pool.try_lock() {
                pool.end_upgrade();
            }
            self.state.gate.open();
        }
    }
}

/// Runs every step before the exec. A refusal has already undone what
/// it did.
pub async fn prepare(
    successor: PathBuf,
    pool: &Arc<Mutex<SessionPool>>,
    state: &Arc<UpgradeState>,
) -> Result<Prepared, Refusal> {
    if !state.replaceable {
        return Err(Refusal::Unsupported(
            "this process embeds the daemon and cannot be replaced by felis-daemon".to_owned(),
        ));
    }
    let Some((listen, socket)) = state.listener() else {
        return Err(Refusal::Unsupported(
            "this daemon serves no listening socket to carry".to_owned(),
        ));
    };
    probe::ask(&successor, None).await?;
    {
        let mut guard = pool.lock().await;
        if guard.draining() {
            return Err(Refusal::Draining);
        }
        if !guard.begin_upgrade() {
            return Err(Refusal::Busy);
        }
    }
    let mut undo = Undo::new(pool, state);
    // A frame already dispatched may hold its permit for as long as its
    // verb runs (a stop waiting for sessions to end), so closing is
    // bounded like every other wait behind the barrier.
    match tokio::time::timeout(BARRIER_TIMEOUT, state.gate.close()).await {
        Ok(true) => {}
        Ok(false) => {
            undo.run().await;
            return Err(Refusal::Busy);
        }
        Err(_elapsed) => {
            undo.run().await;
            return Err(Refusal::Timeout(
                "a request already being served did not finish".into(),
            ));
        }
    }
    info!(successor = %successor.display(), "upgrade: barrier closed");
    match quiesce_and_dump(pool, listen.as_raw_fd(), &mut undo).await {
        Ok((dump, masters)) => {
            let carried = carrier::create(&dump.encode());
            let carried = match carried {
                Ok(fd) => fd,
                Err(err) => {
                    undo.run().await;
                    return Err(Refusal::ProbeFailed(format!("write the dump: {err}")));
                }
            };
            if let Err(refusal) = probe::ask(&successor, Some(&carried)).await {
                undo.run().await;
                return Err(refusal);
            }
            let carrier_fd = carried.as_raw_fd();
            let carried = std::iter::once(listen)
                .chain(masters)
                .chain(std::iter::once(carried))
                .collect();
            Ok(Prepared {
                successor,
                socket,
                carried,
                carrier_fd,
                undo,
            })
        }
        Err(refusal) => {
            undo.run().await;
            Err(refusal)
        }
    }
}

async fn quiesce_and_dump(
    pool: &Arc<Mutex<SessionPool>>,
    listen_fd: RawFd,
    undo: &mut Undo,
) -> Result<(Dump, Vec<OwnedFd>), Refusal> {
    // Settling runs commands queued before the barrier, which can start
    // a teardown or finish a spawn, so the set is taken again until
    // settling changes nothing.
    let mut settled = std::collections::HashSet::new();
    let (handles, next_sequence, next_attachment_id) = loop {
        wait_in_flight(pool).await?;
        let (handles, next_sequence, next_attachment_id) = {
            let guard = pool.lock().await;
            (
                guard.all_handles(),
                guard.peek_sequence(),
                guard.attachment_ids().peek(),
            )
        };
        let mut changed = false;
        for (id, handle) in &handles {
            if settled.insert(*id) {
                changed = true;
                undo.held.push(handle.cmd.clone());
                settle(handle).await?;
            }
        }
        if !changed && pool.lock().await.in_flight() == 0 {
            break (handles, next_sequence, next_attachment_id);
        }
    };
    let mut sessions = Vec::with_capacity(handles.len());
    let mut masters = Vec::with_capacity(handles.len());
    for (id, handle) in handles {
        let (Some(parts), Some(resizer)) = (handle.upgrade.clone(), handle.resizer.clone()) else {
            return Err(Refusal::Unsupported(format!(
                "session {} has no PTY to carry",
                format_session_id(id.0)
            )));
        };
        let quiescer = parts.quiescer.clone();
        let parking = tokio::task::spawn_blocking(move || quiescer.park(PARK_TIMEOUT));
        let pending = undo.pending.insert((parts.quiescer.clone(), parking));
        let parked = (&mut pending.1).await;
        undo.pending = None;
        let parked = parked
            .map_err(|err| Refusal::Timeout(format!("park a session: {err}")))?
            .map_err(|err| {
                Refusal::Timeout(format!(
                    "session {} did not stop in time: {err}",
                    format_session_id(id.0)
                ))
            })?;
        undo.parked.push(parts.quiescer.clone());
        let master = resizer
            .master_fd()
            .try_clone_to_owned()
            .map_err(|err| Refusal::ProbeFailed(format!("duplicate a PTY master: {err}")))?;
        let state = capture(&handle).await?;
        let mut dumped = session_dump(id.0, &handle, &parts, master.as_raw_fd(), parked.unwritten)?;
        dumped.state = state;
        sessions.push(dumped);
        masters.push(master);
    }
    Ok((
        Dump {
            version: DUMP_VERSION,
            listen_fd,
            next_sequence,
            next_attachment_id,
            anim_clock_ms: crate::graphics::anim_now_ms(),
            sessions,
        },
        masters,
    ))
}

/// Spawns already admitted fork after admission, and a session being
/// torn down still owns a child the dump would not name.
async fn wait_in_flight(pool: &Arc<Mutex<SessionPool>>) -> Result<(), Refusal> {
    let settled = pool.lock().await.settled();
    let wait = async {
        loop {
            let woken = settled.notified();
            tokio::pin!(woken);
            woken.as_mut().enable();
            if pool.lock().await.in_flight() == 0 {
                return;
            }
            woken.await;
        }
    };
    tokio::time::timeout(BARRIER_TIMEOUT, wait)
        .await
        .map_err(|_elapsed| Refusal::Timeout("a session spawn or teardown did not settle".into()))
}

/// Waits for every command already queued to the session: input a
/// connection handed over before the barrier must reach the writer
/// before it parks.
async fn settle(handle: &SessionHandle) -> Result<(), Refusal> {
    let settle = async {
        let (done, settled) = oneshot::channel();
        // A session task that ended has nothing queued.
        if handle.cmd.send(SessionCmd::Settle(done)).await.is_ok() {
            let _settled = settled.await;
        }
    };
    tokio::time::timeout(BARRIER_TIMEOUT, settle)
        .await
        .map_err(|_elapsed| Refusal::Timeout("a session did not settle its queue".into()))
}

/// `None` for a session task that already ended: its screen is gone
/// with it, and the successor shows the session blank.
async fn capture(handle: &SessionHandle) -> Result<Option<Box<SessionState>>, Refusal> {
    let capture = async {
        let (reply, state) = oneshot::channel();
        if handle.cmd.send(SessionCmd::Capture(reply)).await.is_err() {
            return None;
        }
        state.await.ok()
    };
    tokio::time::timeout(BARRIER_TIMEOUT, capture)
        .await
        .map_err(|_elapsed| Refusal::Timeout("a session did not hand over its state".into()))
}

fn session_dump(
    id: u128,
    handle: &SessionHandle,
    parts: &UpgradeParts,
    master_fd: RawFd,
    unwritten: Vec<u8>,
) -> Result<SessionDump, Refusal> {
    let child_failed =
        |err: std::io::Error| Refusal::ProbeFailed(format!("read a session child: {err}"));
    let pid = parts
        .child
        .process_id()
        .map_err(child_failed)?
        .ok_or_else(|| Refusal::ProbeFailed("a session child has no pid".into()))?;
    let exit_status = parts
        .child
        .try_wait()
        .map_err(child_failed)?
        .map(std::process::ExitStatus::into_raw);
    let meta = handle.meta_snapshot();
    Ok(SessionDump {
        id: format_session_id(id),
        sequence: meta.sequence.get(),
        child: ChildDump { pid, exit_status },
        master_fd,
        unwritten,
        rows: meta.rows,
        cols: meta.cols,
        pixel_w: meta.pixel_w,
        pixel_h: meta.pixel_h,
        title: meta.title,
        cwd: meta.cwd,
        tags: meta.tags.into_iter().collect(),
        exited: meta.exited || exit_status.is_some(),
        state: None,
    })
}

impl Prepared {
    /// Replaces this process with the successor. Returns only when the
    /// exec failed, after undoing everything, with the refusal to report.
    pub async fn exec(self) -> Refusal {
        let error = match set_all(&self.carried, true) {
            Ok(()) => {
                info!(successor = %self.successor.display(), "upgrade: exec");
                std::process::Command::new(&self.successor)
                    .arg("serve")
                    .arg("--socket")
                    .arg(&self.socket)
                    .arg("--resume-fd")
                    .arg(self.carrier_fd.to_string())
                    .exec()
            }
            Err(err) => err,
        };
        if let Err(err) = set_all(&self.carried, false) {
            warn!(?err, "upgrade: restoring close-on-exec failed");
        }
        drop(self.carried);
        self.undo.run().await;
        Refusal::ProbeFailed(format!("exec {}: {error}", self.successor.display()))
    }

    /// Gives up after a successful prepare, resuming every session.
    pub async fn abandon(self) {
        self.undo.run().await;
    }
}

fn set_all(fds: &[OwnedFd], inheritable: bool) -> std::io::Result<()> {
    for fd in fds {
        felis_transport::inherit::set_inheritable(fd.as_fd(), inheritable)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_upgrade_dropped_midway_reopens_the_gate_and_the_pool() {
        let pool = Arc::new(Mutex::new(SessionPool::new()));
        let state = UpgradeState::replaceable();
        assert!(pool.lock().await.begin_upgrade());
        let undo = Undo::new(&pool, &state);
        assert!(state.gate.close().await);
        drop(undo);
        tokio::time::timeout(Duration::from_secs(5), state.gate.wait_open())
            .await
            .expect("the gate reopens");
        assert!(
            pool.lock().await.begin_upgrade(),
            "the pool left its upgrade, so another may begin"
        );
    }
}

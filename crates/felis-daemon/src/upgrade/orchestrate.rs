//! The predecessor's half: barrier, quiesce, dump, then the platform's
//! hand-over (on Unix the probe and the exec here, on Windows
//! [`super::handover`]).

#[cfg(unix)]
use std::{
    os::fd::{AsFd as _, AsRawFd as _, OwnedFd},
    os::unix::process::{CommandExt as _, ExitStatusExt as _},
    path::PathBuf,
};
use std::{
    path::Path,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use felis_pty::{Parked, Quiescer, Resizer};
use tokio::{
    sync::{Mutex, mpsc, oneshot},
    task::JoinHandle,
};
use tracing::{info, warn};

#[cfg(unix)]
use super::carrier;
use super::{
    Refusal, UpgradeState,
    dump::{DUMP_VERSION, Dump, SessionDump, SessionState, format_session_id},
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
#[cfg(unix)]
pub struct Prepared {
    successor: PathBuf,
    socket: PathBuf,
    /// Duplicates of the listening socket and every master the dump
    /// names, plus the dump's own file. Close-on-exec comes off these
    /// copies only, and only at the exec.
    carried: Vec<OwnedFd>,
    carrier_fd: std::os::fd::RawFd,
    undo: Undo,
}

/// One upgrade's hold on the daemon. A second request is refused
/// before it forks a probe, which could otherwise run while the first
/// has close-on-exec off the descriptors it carries across the exec.
pub(super) struct Claim(Arc<UpgradeState>);

impl Claim {
    fn take(state: &Arc<UpgradeState>) -> Option<Self> {
        state
            .claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self(Arc::clone(state)))
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.0.claimed.store(false, Ordering::Release);
    }
}

/// Armed from the moment the pool stops admitting: a refusal runs it,
/// and so does a drop, so a cancelled or panicking upgrade still
/// leaves the daemon serving.
pub(super) struct Undo {
    pool: Arc<Mutex<SessionPool>>,
    state: Arc<UpgradeState>,
    parked: Vec<Quiescer>,
    /// A park still running on the blocking pool: it outlives a
    /// cancelled upgrade and must be resumed once it ends.
    pending: Option<(Quiescer, JoinHandle<std::io::Result<Parked>>)>,
    /// Session tasks holding their drains since the settle.
    held: Vec<mpsc::Sender<SessionCmd>>,
    armed: bool,
    _claim: Claim,
}

impl Undo {
    fn new(pool: &Arc<Mutex<SessionPool>>, state: &Arc<UpgradeState>, claim: Claim) -> Self {
        Self {
            pool: Arc::clone(pool),
            state: Arc::clone(state),
            parked: Vec::new(),
            pending: None,
            held: Vec::new(),
            armed: true,
            _claim: claim,
        }
    }

    pub(super) async fn run(mut self) {
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

/// Refused for a process embedding the daemon library, such as a
/// test, which would be replaced by a daemon it never meant to become.
pub(super) fn check_replaceable(state: &UpgradeState) -> Result<(), Refusal> {
    if state.replaceable {
        Ok(())
    } else {
        Err(Refusal::Unsupported(
            "this process embeds the daemon and cannot be replaced by felis-daemon".to_owned(),
        ))
    }
}

/// The steps up to and including the barrier, each refusal before the
/// barrier leaving nothing stopped. The returned [`Undo`] resumes the
/// daemon when run or dropped.
pub(super) async fn begin(
    successor: &Path,
    pool: &Arc<Mutex<SessionPool>>,
    state: &Arc<UpgradeState>,
) -> Result<Undo, Refusal> {
    let claim = Claim::take(state).ok_or(Refusal::Busy)?;
    if pool.lock().await.draining() {
        return Err(Refusal::Draining);
    }
    probe::ask_version(successor).await?;
    {
        let mut guard = pool.lock().await;
        if guard.draining() {
            return Err(Refusal::Draining);
        }
        if !guard.begin_upgrade() {
            return Err(Refusal::Busy);
        }
    }
    let undo = Undo::new(pool, state, claim);
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
    Ok(undo)
}

/// Runs every step before the exec. A refusal has already undone what
/// it did.
#[cfg(unix)]
pub async fn prepare(
    successor: PathBuf,
    pool: &Arc<Mutex<SessionPool>>,
    state: &Arc<UpgradeState>,
) -> Result<Prepared, Refusal> {
    check_replaceable(state)?;
    let Some((listen, socket)) = state.listener() else {
        return Err(Refusal::Unsupported(
            "this daemon serves no listening socket to carry".to_owned(),
        ));
    };
    let mut undo = begin(&successor, pool, state).await?;
    match quiesce_and_dump(pool, listen.as_raw_fd(), &mut undo, carry_master).await {
        Ok((dump, masters)) => {
            let carried = carrier::create(&dump.encode());
            let carried = match carried {
                Ok(fd) => fd,
                Err(err) => {
                    undo.run().await;
                    return Err(Refusal::ProbeFailed(format!("write the dump: {err}")));
                }
            };
            if let Err(refusal) = probe::ask_dump(&successor, &carried).await {
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

/// Parks every session and dumps it. `carry` takes what the platform
/// hands over for one parked session and records it in that session's
/// dump.
pub(super) async fn quiesce_and_dump<C>(
    pool: &Arc<Mutex<SessionPool>>,
    listen_fd: i32,
    undo: &mut Undo,
    mut carry: impl FnMut(&Resizer, &UpgradeParts, &mut SessionDump) -> Result<C, Refusal>,
) -> Result<(Dump, Vec<C>), Refusal> {
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
    let mut carried = Vec::with_capacity(handles.len());
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
        let mut dumped = session_dump(id.0, &handle, parked.unwritten);
        carried.push(carry(&resizer, &parts, &mut dumped)?);
        dumped.state = capture(&handle).await?;
        sessions.push(dumped);
    }
    Ok((
        Dump {
            version: DUMP_VERSION,
            listen_fd,
            predecessor: 0,
            creator: 0,
            next_sequence,
            next_attachment_id,
            anim_clock_ms: crate::graphics::anim_now_ms(),
            sessions,
        },
        carried,
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

fn session_dump(id: u128, handle: &SessionHandle, unwritten: Vec<u8>) -> SessionDump {
    let meta = handle.meta_snapshot();
    SessionDump {
        id: format_session_id(id),
        sequence: meta.sequence.get(),
        unwritten,
        rows: meta.rows,
        cols: meta.cols,
        pixel_w: meta.pixel_w,
        pixel_h: meta.pixel_h,
        title: meta.title,
        cwd: meta.cwd,
        spawn_cwd: meta.spawn_cwd,
        tags: meta.tags.into_iter().collect(),
        exited: meta.exited,
        ..SessionDump::default()
    }
}

fn child_failed(err: &std::io::Error) -> Refusal {
    Refusal::ProbeFailed(format!("read a session child: {err}"))
}

/// The PTY master, with the child the successor will wait for.
#[cfg(unix)]
fn carry_master(
    resizer: &Resizer,
    parts: &UpgradeParts,
    dumped: &mut SessionDump,
) -> Result<OwnedFd, Refusal> {
    let master = resizer
        .master_fd()
        .try_clone_to_owned()
        .map_err(|err| Refusal::ProbeFailed(format!("duplicate a PTY master: {err}")))?;
    let pid = parts
        .child
        .process_id()
        .map_err(|err| child_failed(&err))?
        .ok_or_else(|| Refusal::ProbeFailed("a session child has no pid".into()))?;
    let exit_status = parts
        .child
        .try_wait()
        .map_err(|err| child_failed(&err))?
        .map(std::process::ExitStatus::into_raw);
    dumped.master_fd = master.as_raw_fd();
    dumped.child = super::dump::ChildDump { pid, exit_status };
    dumped.exited |= exit_status.is_some();
    Ok(master)
}

/// The pseudoconsole's handles and the child's. Their values in the
/// successor are written once it exists.
#[cfg(windows)]
pub(super) fn carry_console(
    resizer: &Resizer,
    parts: &UpgradeParts,
    dumped: &mut SessionDump,
) -> Result<felis_pty::handover::Handover, Refusal> {
    dumped.exited |= parts
        .child
        .try_wait()
        .map_err(|err| child_failed(&err))?
        .is_some();
    felis_pty::handover::Handover::of(resizer, &parts.child)
        .map_err(|err| Refusal::Unsupported(format!("hand over a session's console: {err}")))
}

#[cfg(unix)]
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

#[cfg(unix)]
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
        let undo = Undo::new(&pool, &state, Claim::take(&state).expect("unclaimed"));
        assert!(state.gate.close().await);
        drop(undo);
        tokio::time::timeout(Duration::from_secs(5), state.gate.wait_open())
            .await
            .expect("the gate reopens");
        assert!(
            pool.lock().await.begin_upgrade(),
            "the pool left its upgrade, so another may begin"
        );
        assert!(Claim::take(&state).is_some(), "the claim was released");
    }

    #[cfg(unix)]
    /// A successor path no probe can run: a refusal other than the one
    /// asserted means the probe was attempted.
    async fn prepare_with_a_listener(
        pool: &Arc<Mutex<SessionPool>>,
        state: &Arc<UpgradeState>,
    ) -> Result<Prepared, Refusal> {
        let listener = std::fs::File::open("/dev/null").expect("/dev/null opens");
        state
            .set_listener(listener.as_fd(), PathBuf::from("/nonexistent/d.sock"))
            .expect("the listener is recorded");
        prepare(PathBuf::from("/nonexistent/felis-daemon"), pool, state).await
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_second_upgrade_is_busy_before_it_probes() {
        let pool = Arc::new(Mutex::new(SessionPool::new()));
        let state = UpgradeState::replaceable();
        let _first = Claim::take(&state).expect("unclaimed");
        let refusal = prepare_with_a_listener(&pool, &state).await.err();
        assert_eq!(refusal, Some(Refusal::Busy));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_draining_daemon_refuses_before_it_probes() {
        let pool = Arc::new(Mutex::new(SessionPool::new()));
        pool.lock().await.start_draining();
        let state = UpgradeState::replaceable();
        let refusal = prepare_with_a_listener(&pool, &state).await.err();
        assert_eq!(refusal, Some(Refusal::Draining));
        assert!(
            Claim::take(&state).is_some(),
            "the refusal released its claim"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_version_probe_releases_the_claim() {
        let pool = Arc::new(Mutex::new(SessionPool::new()));
        let state = UpgradeState::replaceable();
        let refusal = prepare_with_a_listener(&pool, &state).await.err();
        assert!(
            matches!(refusal, Some(Refusal::ProbeFailed(_))),
            "{refusal:?}"
        );
        assert!(Claim::take(&state).is_some());
        assert!(
            pool.lock().await.begin_upgrade(),
            "the pool is left admitting"
        );
    }
}

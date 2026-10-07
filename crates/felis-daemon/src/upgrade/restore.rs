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
    dump::{Dump, DumpError, SessionDump, SessionState, parse_session_id},
};
use crate::{
    SessionId, SessionPool, SpawnedPty,
    graphics::ShmDeferral,
    pool::{ParseCore, Session, StoredNotification},
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
    if !crate::graphics::resume_anim_clock(dump.anim_clock_ms) {
        warn!("upgrade: image animations restart their frame clocks");
    }
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
    let state = dump.state.clone().unwrap_or_else(|| {
        Box::new(SessionState {
            grid: felis_grid::Grid::new(dump.rows, dump.cols),
            ..SessionState::default()
        })
    });
    let SessionState {
        grid,
        parser,
        table_gc,
        images,
        placements,
        saved_primary_placements,
        reassembler,
        shm_segments,
        reported_focus,
        reported_os_dark,
        reported_resize,
        idle_ms,
        last_notification,
    } = *state;
    let core = ParseCore {
        parser,
        grid,
        table_gc,
    };
    let spawned = SpawnedPty::adopt(master, dump.child.pid, dump.child.exit_status, core)
        .map_err(|err| err.to_string())?;
    let mut session = Session::from_spawned(spawned);
    session.images = images;
    session.placements = placements;
    session.saved_primary_placements = saved_primary_placements;
    session.graphics_reassembler = reassembler;
    session.shm_segments =
        ShmDeferral::from_names(shm_segments).ok_or("too many deferred shared memory names")?;
    let now = Instant::now();
    let last_notification = last_notification.map(|n| StoredNotification {
        title: n.title,
        body: n.body,
        urgency: n.urgency,
        at: now
            .checked_sub(Duration::from_millis(n.age_ms))
            .unwrap_or(now),
    });
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
            // A captured grid is the authority, an absent title included;
            // the listed meta only stands in for a screen not carried.
            title: dump.title.clone().filter(|_| dump.state.is_none()),
            cwd: dump.cwd.clone().filter(|_| dump.state.is_none()),
            exited: dump.exited,
            reported_focus,
            reported_os_dark,
            reported_resize,
            idle_for: Duration::from_millis(idle_ms),
            last_notification,
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// A real PTY child whose master the predecessor would carry: its
    /// first owner parked, so the adopter alone reads and writes it.
    fn carried_child(script: &str) -> (OwnedFd, i32, SpawnedPty) {
        let mut cmd = felis_pty::Command::new("/bin/sh");
        cmd.arg("-c").arg(script);
        let spawned = SpawnedPty::spawn(cmd).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        spawned.quiescer.park(Duration::from_secs(2)).unwrap();
        let master = spawned.resizer.master_fd().try_clone_to_owned().unwrap();
        let pid = spawned.child.process_id().unwrap().unwrap();
        (master, pid, spawned)
    }

    fn restored(title: Option<&str>) -> Restored {
        Restored {
            sequence: NonZeroU64::MIN,
            title: title.map(str::to_owned),
            cwd: None,
            exited: false,
            reported_focus: false,
            reported_os_dark: false,
            reported_resize: None,
            idle_for: Duration::ZERO,
            last_notification: None,
        }
    }

    /// While an upgrade holds a session, its exited child stays unreaped:
    /// the dump names it, and a reaped pid may be reused before the
    /// successor signals it. Resuming lets the reap go ahead.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_held_session_does_not_reap_its_exited_child_until_resumed() {
        let (master, pid, _first) = carried_child("sleep 0.5");
        let session = Session::from_spawned(
            SpawnedPty::adopt(master, pid, None, ParseCore::new(24, 80)).unwrap(),
        );
        let pool = Arc::new(Mutex::new(SessionPool::new()));
        let policy = IdlePolicy {
            post_exit_grace: Duration::from_millis(50),
            drain_interval: Duration::from_millis(10),
        };
        spawn_restored(
            &pool,
            session,
            policy,
            SessionId(1),
            0,
            0,
            Vec::new(),
            restored(None),
        )
        .await;
        let cmd = pool.lock().await.all_handles()[0].1.cmd.clone();
        let (done, settled) = tokio::sync::oneshot::channel();
        cmd.send(crate::serve::session_task::SessionCmd::Settle(done))
            .await
            .unwrap();
        settled.await.unwrap();

        tokio::time::sleep(Duration::from_millis(1500)).await;
        let pid = Pid::from_raw(pid).unwrap();
        assert!(exited(pid), "the child has exited");
        let still_ours = rustix::process::waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        );
        assert!(
            matches!(still_ours, Ok(Some(_))),
            "the exited child is still unreaped: {still_ours:?}"
        );
        assert_eq!(
            pool.lock().await.all_handles().len(),
            1,
            "the session is kept"
        );

        cmd.send(crate::serve::session_task::SessionCmd::Resume)
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pool.lock().await.all_handles().is_empty() {
            assert!(Instant::now() < deadline, "the resumed session reaps");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A carried screen decides the restored title, an absent one
    /// included: the listed title may predate a clear the parser saw.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_carried_screen_without_a_title_does_not_regain_the_listed_one() {
        let (master, pid, _first) = carried_child("sleep 30");
        let dump = SessionDump {
            id: crate::upgrade::dump::format_session_id(7),
            sequence: 1,
            child: crate::upgrade::dump::ChildDump {
                pid,
                exit_status: None,
            },
            rows: 24,
            cols: 80,
            title: Some("stale".to_owned()),
            cwd: Some("/stale".to_owned()),
            state: Some(Box::new(SessionState {
                grid: felis_grid::Grid::new(24, 80),
                ..SessionState::default()
            })),
            ..SessionDump::default()
        };
        let pool = Arc::new(Mutex::new(SessionPool::new()));
        register(&pool, IdlePolicy::default(), &dump, master)
            .await
            .unwrap();
        let meta = pool.lock().await.all_handles()[0].1.meta_snapshot();
        let _killed =
            rustix::process::kill_process_group(Pid::from_raw(pid).unwrap(), Signal::KILL);
        assert_eq!(meta.title, None);
        assert_eq!(meta.cwd, None);
    }

    /// A reply the predecessor parsed but never drained reaches the
    /// child after the restore, with no window attached and no further
    /// output from the child to wake the session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_restored_session_answers_a_query_its_predecessor_left_queued() {
        let tmp = tempfile::tempdir().unwrap();
        let reply = tmp.path().join("reply");
        let mut cmd = felis_pty::Command::new("/bin/sh");
        cmd.arg("-c").arg(format!(
            "stty raw -echo; head -c 3 > '{}'; sleep 30",
            reply.display()
        ));
        let spawned = SpawnedPty::spawn(cmd).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        spawned.quiescer.park(Duration::from_secs(2)).unwrap();
        let master = spawned.resizer.master_fd().try_clone_to_owned().unwrap();
        let pid = spawned.child.process_id().unwrap().unwrap();

        let mut core = ParseCore::new(24, 80);
        core.parser.advance(&mut core.grid, b"\x1b[c");
        let session = Session::from_spawned(SpawnedPty::adopt(master, pid, None, core).unwrap());
        let pool = Arc::new(Mutex::new(SessionPool::new()));
        spawn_restored(
            &pool,
            session,
            IdlePolicy::default(),
            SessionId(1),
            0,
            0,
            Vec::new(),
            Restored {
                sequence: NonZeroU64::MIN,
                title: None,
                cwd: None,
                exited: false,
                reported_focus: false,
                reported_os_dark: false,
                reported_resize: None,
                idle_for: Duration::ZERO,
                last_notification: None,
            },
        )
        .await;

        let deadline = Instant::now() + Duration::from_secs(5);
        let got = loop {
            let got = std::fs::read(&reply).unwrap_or_default();
            if got.len() >= 3 || Instant::now() >= deadline {
                break got;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let _killed =
            rustix::process::kill_process_group(Pid::from_raw(pid).unwrap(), Signal::KILL);
        assert_eq!(
            got, b"\x1b[?",
            "the queued device-attributes reply reached the child"
        );
    }
}

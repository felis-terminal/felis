//! Parking a session's PTY threads for an in-place daemon upgrade
//! (`docs/explanation/architecture/overview.md` "In-place upgrade").

use std::{
    io,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::{SinkShared, WriterState, poison};

pub(crate) struct QuiesceShared {
    requested: AtomicBool,
    state: Mutex<ParkState>,
    resume: Condvar,
    #[cfg(unix)]
    wake: crate::platform::WakePipe,
}

#[derive(Default)]
struct ParkState {
    reader_parked: bool,
    writer_parked: bool,
    /// The parked writer's untaken input, in write order, starting at
    /// the byte its last partial write ended at.
    unwritten: Vec<u8>,
    /// The full length of every item behind `unwritten`, which the
    /// writer's pending gauge reaches once nothing is left unseen.
    held_total: u64,
}

impl QuiesceShared {
    #[cfg_attr(
        windows,
        expect(clippy::unnecessary_wraps, reason = "the wake pipe is Unix-only")
    )]
    pub(crate) fn new() -> io::Result<Self> {
        Ok(Self {
            requested: AtomicBool::new(false),
            state: Mutex::new(ParkState::default()),
            resume: Condvar::new(),
            #[cfg(unix)]
            wake: crate::platform::WakePipe::new()?,
        })
    }

    pub(crate) fn requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    #[cfg(unix)]
    pub(crate) fn wake_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.wake.read_end()
    }

    /// The reader's park point, at the top of its loop: nothing it read
    /// is held locally there.
    pub(crate) fn reader_checkpoint(&self) {
        if !self.requested() {
            return;
        }
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.reader_parked = true;
        while self.requested() {
            let Ok(woken) = self.resume.wait(state) else {
                return;
            };
            state = woken;
        }
        state.reader_parked = false;
    }

    /// The writer's park point, between two writes. `held` drains the queue
    /// and reports the unwritten bytes and their items' full length; it runs
    /// on every wake, and the bytes stay the writer's for a refused upgrade
    /// to resume with.
    pub(crate) fn writer_checkpoint(&self, mut held: impl FnMut() -> (Vec<u8>, u64)) {
        if !self.requested() {
            return;
        }
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        (state.unwritten, state.held_total) = held();
        state.writer_parked = true;
        while self.requested() {
            let Ok(woken) = self.resume.wait(state) else {
                return;
            };
            state = woken;
            if self.requested() {
                (state.unwritten, state.held_total) = held();
            }
        }
        state.writer_parked = false;
        state.unwritten.clear();
        state.held_total = 0;
    }
}

/// A session's PTY threads, parked: the reader holds nothing it read,
/// the parser has consumed everything, and the writer sits between two
/// writes.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Parked {
    /// Input queued for the child that the OS has not taken yet.
    pub unwritten: Vec<u8>,
}

/// Parks and resumes one session's PTY threads. Cloned out of a
/// [`crate::PtySession`] before it is split.
#[derive(Clone)]
pub struct Quiescer {
    pub(crate) shared: Arc<QuiesceShared>,
    pub(crate) sink: Arc<SinkShared>,
    pub(crate) writer: Arc<Mutex<WriterState>>,
}

const POLL_INTERVAL: Duration = Duration::from_millis(1);

impl Quiescer {
    /// Parks the reader, waits for the parser to consume what was read, and
    /// parks the writer within `timeout`, else resumes them with `TimedOut`.
    /// The caller stops queueing input first: a write queued while parking is
    /// parked, not refused. Windows reports `Unsupported`: nothing wakes its
    /// reader's blocking `ReadFile`.
    pub fn park(&self, timeout: Duration) -> io::Result<Parked> {
        if cfg!(windows) {
            return Err(io::Error::from(io::ErrorKind::Unsupported));
        }
        self.shared.requested.store(true, Ordering::Release);
        #[cfg(unix)]
        if let Err(err) = self.shared.wake.signal() {
            self.resume()?;
            return Err(err);
        }
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(parked) = self.try_parked()? {
                return Ok(parked);
            }
            if Instant::now() >= deadline {
                self.resume()?;
                return Err(io::Error::from(io::ErrorKind::TimedOut));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Resumes threads parked by [`park`](Self::park).
    pub fn resume(&self) -> io::Result<()> {
        // Drained before the flag clears: a reader polling a still
        // readable wake end with the flag already down would spin.
        #[cfg(unix)]
        self.shared.wake.drain()?;
        let state = self.shared.state.lock().map_err(poison)?;
        self.shared.requested.store(false, Ordering::Release);
        drop(state);
        self.shared.resume.notify_all();
        Ok(())
    }

    fn try_parked(&self) -> io::Result<Option<Parked>> {
        let (reader_done, parser_done) = {
            let sink = self.sink.state.lock().map_err(poison)?;
            // A reader past EOF has left its loop and holds nothing.
            let reader_done = sink.eof;
            let parser_done = sink.parser_gone || (sink.filling.is_empty() && sink.parser_parked);
            (reader_done, parser_done)
        };
        let (writer_gone, pending) = {
            let writer = self.writer.lock().map_err(poison)?;
            (writer.gone, writer.pending())
        };
        let state = self.shared.state.lock().map_err(poison)?;
        let reader_ok = state.reader_parked || reader_done;
        let writer_seen_all = state.writer_parked && pending == state.held_total;
        if state.writer_parked && !writer_seen_all {
            self.shared.resume.notify_all();
        }
        let writer_ok = writer_gone || pending == 0 || writer_seen_all;
        if !(reader_ok && parser_done && writer_ok) {
            return Ok(None);
        }
        Ok(Some(Parked {
            unwritten: if state.writer_parked {
                state.unwritten.clone()
            } else {
                Vec::new()
            },
        }))
    }
}

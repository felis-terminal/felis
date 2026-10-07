//! Self-hosted async PTY layer: Unix `posix_openpt` and Windows `ConPTY`.
//!
//! Bridges blocking OS PTY interfaces to Tokio via dedicated threads and swap
//! buffers (`docs/explanation/implementation.md` "PTY layer (self-hosted)").

use std::{
    io::{self, Read, Write},
    pin::Pin,
    sync::{Arc, Condvar, Mutex},
    task::{Context, Poll, Waker},
    thread::{self, JoinHandle},
};

use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
};

mod command;
mod quiesce;
#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

pub use command::{Command, Size, env_bytes, env_from_bytes};
pub use quiesce::{Parked, Quiescer};

/// Pending-byte ceiling for the reader ⇄ parser handoff: the reader
/// parks once this many unparsed bytes are buffered. A flood amortizes
/// the park/wake pair over ~a megabyte; per-session memory is bounded
/// at roughly twice this (the two swap buffers' high-water marks).
const READ_PENDING_CAP: usize = 1024 * 1024;

/// Pending-byte ceiling on writes handed to [`PtyWriter`] before OS pickup.
///
/// Parks the async writer when a child stops reading stdin, bounding daemon
/// memory. Mirrors [`READ_PENDING_CAP`]; writes larger than the cap still pass.
pub const PTY_WRITE_PENDING_CAP: usize = 1024 * 1024;

/// Maximum bytes per `read(2)` against the PTY master. 64 KiB matches
/// the kernel's pipe buffer ceiling on Linux and macOS, so a long `cat`
/// completes in a handful of reads (docs/reference/ipc.md, batched grid
/// wire ops).
const READ_BUFFER_SIZE: usize = 64 * 1024;

/// Errors surfaced by [`spawn`] before the child is running.
#[derive(Debug, Error)]
pub enum PtyError {
    #[error("open PTY: {0}")]
    OpenPty(String),
    #[error("spawn child: {0}")]
    Spawn(String),
    #[error("spawn {thread} thread: {source}")]
    ThreadSpawn {
        thread: &'static str,
        #[source]
        source: io::Error,
    },
}

pub struct PtySession {
    reader: PtyReader,
    writer: PtyWriter,
    master: platform::Master,
    child: platform::Child,
    quiescer: Quiescer,
}

impl PtySession {
    /// The handle that parks this session's threads, taken before
    /// [`split`](Self::split) consumes the session.
    #[must_use]
    pub fn quiescer(&self) -> Quiescer {
        self.quiescer.clone()
    }

    #[must_use]
    pub fn split(self) -> (PtyReader, PtyWriter, ChildHandle, Resizer) {
        let Self {
            reader,
            writer,
            master,
            child,
            quiescer: _,
        } = self;
        (
            reader,
            writer,
            ChildHandle {
                child: Mutex::new(child),
            },
            Resizer { master },
        )
    }
}

/// Dropping this does *not* end the child, and neither does dropping
/// the [`Resizer`]: the reader thread holds its own dup of the master
/// until EOF, and EOF is the child exiting. A teardown that wants the
/// child gone says so through [`hangup`](ChildHandle::hangup) or
/// [`kill`](ChildHandle::kill).
pub struct ChildHandle {
    child: Mutex<platform::Child>,
}

enum Teardown {
    Hangup,
    Kill,
}

impl ChildHandle {
    /// `SIGHUP` to the process group on Unix, `TerminateProcess` on
    /// Windows. `Ok(false)` means the child had already exited and
    /// nothing was sent.
    pub fn hangup(&self) -> io::Result<bool> {
        self.signal_if_running(&Teardown::Hangup)
    }

    /// `SIGKILL` on Unix, `TerminateProcess` on Windows. `Ok(false)`
    /// means the child was already gone.
    pub fn kill(&self) -> io::Result<bool> {
        self.signal_if_running(&Teardown::Kill)
    }

    /// Liveness check and signal under one lock acquisition: a reaped
    /// pid may already be someone else's.
    fn signal_if_running(&self, what: &Teardown) -> io::Result<bool> {
        let mut guard = self.child.lock().map_err(poison)?;
        if guard.try_wait()?.is_some() {
            return Ok(false);
        }
        match what {
            Teardown::Hangup => guard.hangup()?,
            Teardown::Kill => guard.kill()?,
        }
        Ok(true)
    }

    pub fn try_wait(&self) -> io::Result<Option<std::process::ExitStatus>> {
        let mut guard = self.child.lock().map_err(poison)?;
        guard.try_wait()
    }

    /// The child's pid, which an in-place upgrade's dump carries.
    #[cfg(unix)]
    pub fn process_id(&self) -> io::Result<Option<i32>> {
        let guard = self.child.lock().map_err(poison)?;
        Ok(guard.process_id())
    }
}

pub struct Resizer {
    master: platform::Master,
}

impl Resizer {
    pub fn resize(&self, size: Size) -> io::Result<()> {
        self.master.resize(size)
    }

    /// The pgid the kernel routes terminal input to (`tcgetpgrp` on the
    /// master), which the daemon resolves to a program name for the
    /// session listing. `None` when the platform cannot report it
    /// (Windows) or nothing holds the foreground.
    #[must_use]
    pub fn foreground_pgrp(&self) -> Option<i32> {
        self.master.foreground_pgrp()
    }

    /// The PTY master, which an in-place upgrade carries across `execve`.
    #[cfg(unix)]
    #[must_use]
    pub fn master_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.master.as_fd()
    }
}

/// The reader thread's end-of-stream report to the async
/// [`PtyReader`]. No bytes cross here; the [`ByteSink`] owns those.
struct LifecycleState {
    eof: bool,
    /// Surfaced once, before `eof`: consumers see the error, then a
    /// clean EOF.
    err: Option<io::Error>,
    waker: Option<Waker>,
}

/// The reader ⇄ parser handoff: the reader appends into `filling` and
/// the parse thread ([`pump_sink`]) takes the whole accumulation in one
/// `mem::swap`. Two `Vec`s swapped rather than one compacted: each side
/// touches only its own buffer, and the high-water capacity is reused.
struct SinkShared {
    state: Mutex<SinkState>,
    /// The reader parks here when `filling` is at [`READ_PENDING_CAP`];
    /// the parser signals after a swap. This bounds per-session memory
    /// and preserves child backpressure: a flooding child fills the cap
    /// plus the kernel queue, then blocks in `write(2)`.
    space: Condvar,
    data: Condvar,
}

// Four independent flags that flip on different sides of the lock.
#[expect(clippy::struct_excessive_bools)]
struct SinkState {
    filling: Vec<u8>,
    /// Checked only after `filling` drains so buffered bytes still get
    /// parsed.
    eof: bool,
    producer_parked: bool,
    parser_parked: bool,
    /// Set by the parse thread's drop guard on every exit, panic
    /// included ([`pump_sink`]).
    parser_gone: bool,
}

/// The session's lifecycle half: it yields no bytes, only the end of
/// the stream.
pub struct PtyReader {
    lifecycle: Arc<Mutex<LifecycleState>>,
    /// Never joined: it blocks in a kernel `read` that only dropping
    /// the master fd unblocks, and the parent drops that only after
    /// this reader. Leaking the thread to the OS beats blocking daemon
    /// shutdown on the syscall.
    _reader_thread: JoinHandle<()>,
}

impl AsyncRead for PtyReader {
    /// Pending until the stream ends, then an end-of-stream read.
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        _buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // Participate in Tokio's cooperative budget, as a bounded mpsc
        // would. Past the end of the stream this future is Ready on
        // every poll, so a `select!` loop that keeps the arm armed
        // never yields and starves the daemon's connection pumps (whose
        // `NextGridFrame` pulls pace frame emission).
        let coop = std::task::ready!(tokio::task::coop::poll_proceed(cx));
        let Ok(mut state) = self.lifecycle.lock() else {
            coop.made_progress();
            return Poll::Ready(Err(io::Error::other("pty read state poisoned")));
        };
        if let Some(err) = state.err.take() {
            coop.made_progress();
            return Poll::Ready(Err(err));
        }
        if state.eof {
            coop.made_progress();
            return Poll::Ready(Ok(()));
        }
        // Registered under the same lock as the `eof` check, so a report
        // landing between the two cannot lose the wakeup.
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

/// A caller-side admission token carried with the bytes and released
/// once the OS writer has taken them: the daemon's per-session input
/// budget rides here so a reservation covers the whole path, not just
/// the enqueue.
pub type WriteReservation = Box<dyn Send + 'static>;

/// What [`PtyWriter::write_owned`] did with the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    Queued,
    /// The unreserved gauge stands at [`PTY_WRITE_PENDING_CAP`], so
    /// these bytes were dropped rather than queued.
    Dropped {
        /// Single saturation notice for this episode, true only on opening.
        /// Claimed under the drop decision lock so gauge drains cannot spend
        /// the next episode's notice early.
        report: bool,
    },
}

impl WriteOutcome {
    /// Whether the bytes reached the queue.
    #[must_use]
    pub const fn queued(self) -> bool {
        matches!(self, Self::Queued)
    }
}

struct WriteItem {
    buf: Vec<u8>,
    /// Held for drop (releasing before the OS write would bypass the caller's
    /// admission budget) and read as a flag to exclude admitted bytes from
    /// the unreserved gauge.
    reservation: Option<WriteReservation>,
}

/// Accounting between the async writer and writer thread. `written` advances
/// only after `write_all` returns, so `queued - written` tracks bytes not yet
/// taken by the OS: the gauge read by pending cap and [`AsyncWrite::poll_flush`].
struct WriterState {
    queued: u64,
    written: u64,
    /// Queued-but-unwritten bytes that carried no reservation, gauged
    /// apart from `queued`: reserved input was already charged against
    /// the daemon's per-session budget, and counting it here would let
    /// one admitted 16 MiB paste drop every mouse report and every
    /// answer the session owes the child while it drains.
    unreserved_pending: u64,
    /// Whether this saturation episode has already handed out the one
    /// notice [`WriteOutcome::Dropped`] carries. Cleared when the
    /// unreserved gauge empties, which is what makes the next drop a
    /// new episode.
    drop_reported: bool,
    /// The first failure, surfaced once to whoever polls next.
    err: Option<io::Error>,
    /// The writer thread is not coming back: nothing further will drain.
    gone: bool,
    /// One slot suffices: `AsyncWrite` needs `&mut PtyWriter`, so write
    /// and flush never wait concurrently.
    waker: Option<Waker>,
}

impl WriterState {
    const fn pending(&self) -> u64 {
        self.queued.saturating_sub(self.written)
    }

    /// Give back unreserved bytes the writer took (or abandoned), and
    /// end the saturation episode once none are left.
    const fn release_unreserved(&mut self, len: u64) {
        self.unreserved_pending = self.unreserved_pending.saturating_sub(len);
        if self.unreserved_pending == 0 {
            self.drop_reported = false;
        }
    }
}

fn write_state_poisoned() -> io::Error {
    io::Error::other("pty write state poisoned")
}

fn writer_gone() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "pty writer thread exited")
}

pub struct PtyWriter {
    /// `None` past [`AsyncWrite::poll_shutdown`]: closing the channel
    /// is what ends the writer thread, and a shut-down writer must
    /// refuse further bytes rather than queue them for a thread that is
    /// on its way out.
    tx: Option<UnboundedSender<WriteItem>>,
    state: Arc<Mutex<WriterState>>,
    /// Not joined (that would block a drop on the daemon's runtime); it
    /// exits once `tx`, which drops before this field, closes the
    /// channel.
    _writer_thread: JoinHandle<()>,
}

impl PtyWriter {
    /// Enqueue without waiting, for callers that must not park.
    ///
    /// Reserved bytes always enqueue. Unreserved bytes drop when at
    /// [`PTY_WRITE_PENDING_CAP`], returning [`WriteOutcome::Dropped`] with
    /// the episode's single log notice.
    pub fn write_owned(
        &self,
        buf: Vec<u8>,
        reservation: Option<WriteReservation>,
    ) -> io::Result<WriteOutcome> {
        let len = buf.len() as u64;
        {
            let mut state = self.state.lock().map_err(|_| write_state_poisoned())?;
            if let Some(err) = state.err.take() {
                return Err(err);
            }
            if state.gone {
                return Err(writer_gone());
            }
            if reservation.is_none() {
                if state.unreserved_pending >= PTY_WRITE_PENDING_CAP as u64 {
                    let report = !state.drop_reported;
                    state.drop_reported = true;
                    return Ok(WriteOutcome::Dropped { report });
                }
                state.unreserved_pending += len;
            }
            state.queued += len;
        }
        self.enqueue(WriteItem { buf, reservation })?;
        Ok(WriteOutcome::Queued)
    }

    /// Bytes queued but not yet taken by the OS writer.
    #[must_use]
    pub fn pending_bytes(&self) -> u64 {
        self.state
            .lock()
            .map_or(0, |state: std::sync::MutexGuard<'_, WriterState>| {
                state.pending()
            })
    }

    /// Hands the item to the writer thread, unwinding the `queued`
    /// charge when the thread is already gone (or this writer is shut
    /// down) so a later flush cannot wait on bytes nobody will write.
    fn enqueue(&self, item: WriteItem) -> io::Result<()> {
        let len = item.buf.len() as u64;
        let unreserved = item.reservation.is_none();
        let delivered = self.tx.as_ref().is_some_and(|tx| tx.send(item).is_ok());
        if !delivered {
            if let Ok(mut state) = self.state.lock() {
                state.queued = state.queued.saturating_sub(len);
                if unreserved {
                    state.release_unreserved(len);
                }
                state.gone = true;
            }
            return Err(writer_gone());
        }
        Ok(())
    }
}

impl AsyncWrite for PtyWriter {
    /// Over the cap this registers the waker and returns `Pending`
    /// *without* copying `buf`: `AsyncWrite` forbids consuming a buffer
    /// a `Pending` write did not accept.
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        {
            let Ok(mut state) = self.state.lock() else {
                return Poll::Ready(Err(write_state_poisoned()));
            };
            if let Some(err) = state.err.take() {
                return Poll::Ready(Err(err));
            }
            if state.gone {
                return Poll::Ready(Err(writer_gone()));
            }
            if state.unreserved_pending >= PTY_WRITE_PENDING_CAP as u64 {
                state.waker = Some(cx.waker().clone());
                return Poll::Pending;
            }
            state.queued += buf.len() as u64;
            state.unreserved_pending += buf.len() as u64;
        }
        match self.enqueue(WriteItem {
            buf: buf.to_vec(),
            reservation: None,
        }) {
            Ok(()) => Poll::Ready(Ok(buf.len())),
            Err(err) => Poll::Ready(Err(err)),
        }
    }

    /// Completes only once every queued byte has come back from
    /// `write_all`, or the write failed: callers that report a write
    /// error (`felis sessions send`) need "delivered", not "queued".
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let Ok(mut state) = self.state.lock() else {
            return Poll::Ready(Err(write_state_poisoned()));
        };
        if let Some(err) = state.err.take() {
            return Poll::Ready(Err(err));
        }
        if state.pending() == 0 {
            return Poll::Ready(Ok(()));
        }
        if state.gone {
            return Poll::Ready(Err(writer_gone()));
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }

    /// Flush, then close: dropping the sender is what lets the writer
    /// thread exit, and it is also what makes a write after shutdown
    /// fail instead of queueing bytes for a thread that is leaving.
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let flushed = std::task::ready!(self.as_mut().poll_flush(cx));
        // Closed even when the flush reported the child's failure: the
        // writer is finished either way, and leaving the channel open
        // would let the next write queue bytes nobody will take.
        self.tx = None;
        Poll::Ready(flushed)
    }
}

/// Chunk sink invoked on the dedicated parse thread, one call per swap.
///
/// Blocking inside the sink provides backpressure: the reader parks at
/// `READ_PENDING_CAP`, filling kernel PTY buffers and throttling child writes.
pub type ByteSink = Box<dyn FnMut(&[u8]) + Send>;

/// Spawn `command` on a fresh PTY with its output delivered to `sink`
/// on a dedicated parse thread. EOF on the returned [`PtyReader`] is
/// ordered after the sink's last call.
// By value: consuming the `Command` keeps a caller from re-spawning a
// stale env snapshot by accident.
#[allow(clippy::needless_pass_by_value)]
pub fn spawn(command: Command, size: Size, sink: ByteSink) -> Result<PtySession, PtyError> {
    let (master, child) = platform::open_and_spawn(&command, size)?;
    session_over(master, child, sink)
}

/// Resumes a session whose PTY master and child this process already
/// holds: an in-place upgrade's successor adopting what `execve`
/// carried over. `exit_status` is the raw wait status of a child the
/// predecessor already reaped, so its pid is never signaled or waited
/// on again.
#[cfg(unix)]
pub fn adopt(
    master: std::os::fd::OwnedFd,
    pid: i32,
    exit_status: Option<i32>,
    sink: ByteSink,
) -> Result<PtySession, PtyError> {
    use std::os::unix::process::ExitStatusExt;
    let master = platform::adopt_master(master)
        .map_err(|e| PtyError::OpenPty(format!("adopt master: {e}")))?;
    let child = platform::Child::adopted(pid, exit_status.map(std::process::ExitStatus::from_raw))
        .map_err(|e| PtyError::Spawn(format!("adopt child: {e}")))?;
    session_over(master, child, sink)
}

fn session_over(
    master: platform::Master,
    child: platform::Child,
    sink: ByteSink,
) -> Result<PtySession, PtyError> {
    let (reader, writer, quiescer) = spawn_io_threads(&master, sink)?;
    Ok(PtySession {
        reader,
        writer,
        master,
        child,
        quiescer,
    })
}

fn spawn_io_threads(
    master: &platform::Master,
    sink: ByteSink,
) -> Result<(PtyReader, PtyWriter, Quiescer), PtyError> {
    let quiesce = Arc::new(
        quiesce::QuiesceShared::new()
            .map_err(|e| PtyError::OpenPty(format!("quiesce wake pipe: {e}")))?,
    );
    let mut sync_reader = master
        .clone_reader(&quiesce)
        .map_err(|e| PtyError::OpenPty(format!("clone reader: {e}")))?;
    let sync_writer = master
        .clone_writer(&quiesce)
        .map_err(|e| PtyError::OpenPty(format!("clone writer: {e}")))?;

    let lifecycle = Arc::new(Mutex::new(LifecycleState {
        eof: false,
        err: None,
        waker: None,
    }));

    let sink_shared = Arc::new(SinkShared {
        state: Mutex::new(SinkState {
            filling: Vec::new(),
            eof: false,
            producer_parked: false,
            parser_parked: false,
            parser_gone: false,
        }),
        space: Condvar::new(),
        data: Condvar::new(),
    });
    let parser_shared = Arc::clone(&sink_shared);
    thread::Builder::new()
        .name("felis-pty-parser".into())
        .spawn(move || pump_sink(&parser_shared, sink))
        .map_err(|source| PtyError::ThreadSpawn {
            thread: "parser",
            source,
        })?;

    let reader_lifecycle = Arc::clone(&lifecycle);
    let reader_sink = Arc::clone(&sink_shared);
    let reader_quiesce = Arc::clone(&quiesce);
    let reader_handle = thread::Builder::new()
        .name("felis-pty-reader".into())
        .spawn(move || {
            pump_reads(
                &mut sync_reader,
                &reader_lifecycle,
                &reader_sink,
                &reader_quiesce,
            );
        })
        .map_err(|source| PtyError::ThreadSpawn {
            thread: "reader",
            source,
        })?;

    let writer = spawn_writer_with(sync_writer, Arc::clone(&quiesce))?;
    let quiescer = Quiescer {
        shared: quiesce,
        sink: sink_shared,
        writer: Arc::clone(&writer.state),
    };
    Ok((
        PtyReader {
            lifecycle,
            _reader_thread: reader_handle,
        },
        writer,
        quiescer,
    ))
}

/// The writer half over any blocking sink, so the byte gauge can be
/// exercised against a sink the test controls rather than a child.
#[cfg(test)]
fn spawn_writer<W: Write + Send + 'static>(sync_writer: W) -> Result<PtyWriter, PtyError> {
    let quiesce = quiesce::QuiesceShared::new()
        .map_err(|e| PtyError::OpenPty(format!("quiesce wake pipe: {e}")))?;
    spawn_writer_with(sync_writer, Arc::new(quiesce))
}

fn spawn_writer_with<W: Write + Send + 'static>(
    mut sync_writer: W,
    quiesce: Arc<quiesce::QuiesceShared>,
) -> Result<PtyWriter, PtyError> {
    let (write_tx, mut write_rx): (UnboundedSender<WriteItem>, UnboundedReceiver<WriteItem>) =
        mpsc::unbounded_channel();
    // Unbounded in count on purpose: the byte gauge in `WriterState` is
    // the bound, and a bounded channel would add a second wake path
    // saying the same thing.
    let state = Arc::new(Mutex::new(WriterState {
        queued: 0,
        written: 0,
        unreserved_pending: 0,
        drop_reported: false,
        err: None,
        gone: false,
        waker: None,
    }));
    let thread_state = Arc::clone(&state);
    let writer_thread = thread::Builder::new()
        .name("felis-pty-writer".into())
        .spawn(move || {
            // Items received while parked, the first one possibly
            // part-written: `(item, bytes of it already written)`.
            let mut held: std::collections::VecDeque<(WriteItem, usize)> =
                std::collections::VecDeque::new();
            loop {
                let (item, mut done) = match held.pop_front() {
                    Some(entry) => entry,
                    None => match write_rx.blocking_recv() {
                        Some(item) => (item, 0),
                        None => break,
                    },
                };
                let len = item.buf.len() as u64;
                let unreserved = item.reservation.is_none();
                // One item written to completion before the next
                // preserves message boundaries on the input side (paste
                // bursts, mode-set sequences); a park may split one.
                let outcome = loop {
                    if done == item.buf.len() {
                        break Some(Ok(()));
                    }
                    if quiesce.requested() {
                        break None;
                    }
                    match sync_writer.write(&item.buf[done..]) {
                        Ok(0) => break Some(Err(io::Error::from(io::ErrorKind::WriteZero))),
                        Ok(n) => done += n,
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                        Err(err) => break Some(Err(err)),
                    }
                };
                let Some(outcome) = outcome else {
                    held.push_front((item, done));
                    quiesce.writer_checkpoint(|| {
                        while let Ok(next) = write_rx.try_recv() {
                            held.push_back((next, 0));
                        }
                        let unwritten = held
                            .iter()
                            .flat_map(|(held_item, from)| held_item.buf[*from..].iter().copied())
                            .collect();
                        let total = held
                            .iter()
                            .map(|(held_item, _)| held_item.buf.len() as u64)
                            .sum();
                        (unwritten, total)
                    });
                    continue;
                };
                let failed = outcome.is_err();
                // Released only here: an admission budget that freed at
                // enqueue would bound nothing.
                drop(item);
                let waker = if let Ok(mut state) = thread_state.lock() {
                    state.written += len;
                    if unreserved {
                        state.release_unreserved(len);
                    }
                    if let Err(err) = outcome {
                        tracing::warn!(?err, "pty writer thread: write_all failed");
                        state.err = Some(err);
                    }
                    state.waker.take()
                } else {
                    None
                };
                if let Some(waker) = waker {
                    waker.wake();
                }
                if failed {
                    break;
                }
            }
            // Whatever is still in the channel is abandoned with it, so
            // it is settled here rather than left standing: the gauge
            // reports bytes the OS has not taken *yet*, and bytes
            // nobody will ever take would make it report a backlog for
            // the life of the session.
            drop(write_rx);
            // A writer waiting on a flush or on space must not outlive
            // the thread that was going to wake it.
            let waker = if let Ok(mut state) = thread_state.lock() {
                state.gone = true;
                state.written = state.queued;
                state.unreserved_pending = 0;
                state.drop_reported = false;
                state.waker.take()
            } else {
                None
            };
            if let Some(waker) = waker {
                waker.wake();
            }
        })
        .map_err(|source| PtyError::ThreadSpawn {
            thread: "writer",
            source,
        })?;

    Ok(PtyWriter {
        tx: Some(write_tx),
        state,
        _writer_thread: writer_thread,
    })
}

fn pump_reads<R: Read>(
    reader: &mut R,
    lifecycle: &Mutex<LifecycleState>,
    sink_shared: &SinkShared,
    quiesce: &quiesce::QuiesceShared,
) {
    let mut buf = vec![0u8; READ_BUFFER_SIZE];
    let outcome = loop {
        quiesce.reader_checkpoint();
        match reader.read(&mut buf) {
            Ok(0) => break Ok(()),
            Ok(n) => {
                let Ok(mut state) = sink_shared.state.lock() else {
                    return;
                };
                while state.filling.len() >= READ_PENDING_CAP && !state.parser_gone {
                    state.producer_parked = true;
                    let Ok(unparked) = sink_shared.space.wait(state) else {
                        return;
                    };
                    state = unparked;
                    state.producer_parked = false;
                }
                if state.parser_gone {
                    // Fall through to the EOF tail: a bare return would
                    // leave the lifecycle `PtyReader` awaiting forever.
                    break Ok(());
                }
                state.filling.extend_from_slice(&buf[..n]);
                let wake = state.parser_parked;
                drop(state);
                if wake {
                    sink_shared.data.notify_one();
                }
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => break Err(err),
        }
    };
    if let Ok(mut state) = sink_shared.state.lock() {
        state.eof = true;
        let wake = state.parser_parked;
        drop(state);
        if wake {
            sink_shared.data.notify_one();
        }
    }
    // Delay lifecycle EOF until the parse thread finishes draining its final
    // swap, ensuring an exiting shell's trailing output is not dropped.
    // The parse thread's drop guard sets `parser_gone` and notifies `space`.
    if let Ok(mut state) = sink_shared.state.lock() {
        while !state.parser_gone {
            state.producer_parked = true;
            let Ok(woken) = sink_shared.space.wait(state) else {
                break;
            };
            state = woken;
            state.producer_parked = false;
        }
    }
    let Ok(mut state) = lifecycle.lock() else {
        return;
    };
    state.eof = true;
    state.err = outcome.err();
    let waker = state.waker.take();
    drop(state);
    if let Some(waker) = waker {
        waker.wake();
    }
}

fn pump_sink(shared: &SinkShared, mut sink: ByteSink) {
    /// Marks the parser gone on every exit: a `sink` panic unwinds
    /// through here, so a reader parked on `space` against a full
    /// buffer is released instead of waiting forever for a swap.
    struct ParserGoneGuard<'a>(&'a SinkShared);
    impl Drop for ParserGoneGuard<'_> {
        fn drop(&mut self) {
            if let Ok(mut state) = self.0.state.lock() {
                state.parser_gone = true;
                let unpark = state.producer_parked;
                drop(state);
                if unpark {
                    self.0.space.notify_one();
                }
            }
        }
    }
    // One bounded back-off lets the reader accumulate more before the
    // swap. Without it the parser swaps ~1 KiB slices and the threads
    // contend the mutex per kernel chunk (contended `os_unfair_lock`
    // parks threads, costing more than the split saves). 50 µs is
    // invisible next to the ~24 ms keypress→display budget.
    const MIN_SWAP: usize = 8 * 1024;
    let _guard = ParserGoneGuard(shared);
    let mut draining = Vec::new();
    loop {
        let mut state = {
            let Ok(mut state) = shared.state.lock() else {
                return;
            };
            while state.filling.is_empty() && !state.eof {
                state.parser_parked = true;
                let Ok(woken) = shared.data.wait(state) else {
                    return;
                };
                state = woken;
                state.parser_parked = false;
            }
            state
        };
        if state.filling.len() < MIN_SWAP && !state.eof {
            drop(state);
            thread::sleep(std::time::Duration::from_micros(50));
            let Ok(relocked) = shared.state.lock() else {
                return;
            };
            state = relocked;
        }
        if state.filling.is_empty() {
            if state.eof {
                return;
            }
            continue;
        }
        std::mem::swap(&mut state.filling, &mut draining);
        let unpark = state.producer_parked;
        drop(state);
        if unpark {
            shared.space.notify_one();
        }
        sink(&draining);
        draining.clear();
    }
}

fn poison<T>(_: std::sync::PoisonError<T>) -> io::Error {
    io::Error::other("pty mutex poisoned")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use tokio::io::AsyncReadExt;

    fn small_size() -> Size {
        Size {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    /// From `ComSpec` with the canonical location as a fallback, so the
    /// spawn resolves under a stripped-down environment whose `PATH`
    /// lacks `System32`.
    #[cfg(windows)]
    fn comspec() -> std::ffi::OsString {
        std::env::var_os("ComSpec").unwrap_or_else(|| r"C:\Windows\System32\cmd.exe".into())
    }

    #[cfg(unix)]
    fn shell_echo(marker: &str) -> Command {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", &format!("printf '{marker}\\n'; exit 0")]);
        cmd
    }

    #[cfg(windows)]
    fn shell_echo(marker: &str) -> Command {
        let mut cmd = Command::new(comspec());
        cmd.args(["/c", &format!("echo {marker}")]);
        cmd
    }

    fn collector() -> (Arc<Mutex<Vec<u8>>>, ByteSink) {
        let collected = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink_side = Arc::clone(&collected);
        let sink: ByteSink = Box::new(move |bytes: &[u8]| {
            sink_side
                .lock()
                .expect("sink collector lock")
                .extend_from_slice(bytes);
        });
        (collected, sink)
    }

    fn collected_text(collected: &Mutex<Vec<u8>>) -> String {
        String::from_utf8_lossy(&collected.lock().expect("collector lock")).into_owned()
    }

    async fn sink_until(collected: &Mutex<Vec<u8>>, needle: &str) -> String {
        for _ in 0..200 {
            let text = collected_text(collected);
            if text.contains(needle) {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        collected_text(collected)
    }

    /// An unresolvable program surfaces as `PtyError::Spawn`, not a
    /// hang or a panic.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawning_a_nonexistent_program_reports_spawn_error() {
        let (_collected, sink) = collector();
        let result = spawn(Command::new("/nonexistent/felis-probe"), small_size(), sink);
        match result {
            Err(PtyError::Spawn(_)) => {}
            Err(other) => panic!("expected PtyError::Spawn, got {other:?}"),
            Ok(_) => panic!("expected PtyError::Spawn, got a live session"),
        }
    }

    /// The initial winsize reaches the kernel and a `Resizer::resize`
    /// is visible to a child that asks again (`stty size`).
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn initial_winsize_and_resize_reach_the_child() {
        use tokio::io::AsyncWriteExt;

        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "stty size; read _x; stty size; exit 0"]);
        let (collected, sink) = collector();
        let session = spawn(cmd, small_size(), sink).expect("spawn");
        let (_reader, mut writer, _child, resizer) = session.split();

        let first = sink_until(&collected, "24 80").await;
        assert!(first.contains("24 80"), "initial winsize: {first:?}");

        resizer
            .resize(Size {
                rows: 30,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize");
        writer.write_all(b"\n").await.expect("wake the child");
        let second = sink_until(&collected, "30 100").await;
        assert!(second.contains("30 100"), "post-resize winsize: {second:?}");
    }

    /// `PATH` for `env_clear`ed test fixtures.
    ///
    /// Combines standard FHS paths (`/bin:/usr/bin`) with the host `PATH` so
    /// non-FHS systems (such as NixOS) can resolve test binaries.
    #[cfg(unix)]
    fn fixture_path() -> std::ffi::OsString {
        let mut path = std::ffi::OsString::from("/bin:/usr/bin");
        if let Some(host) = std::env::var_os("PATH") {
            path.push(":");
            path.push(host);
        }
        path
    }

    #[cfg(unix)]
    fn blocks_forever() -> Command {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "sleep 60"]);
        cmd
    }

    #[cfg(windows)]
    fn blocks_forever() -> Command {
        let mut cmd = Command::new(comspec());
        cmd.args(["/c", "pause"]);
        cmd
    }

    /// The child's output reaches the sink and none of it leaks into
    /// the reader half.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sink_receives_output_and_reader_carries_no_data() {
        let (collected, sink) = collector();
        let session = spawn(shell_echo("felis-sink"), small_size(), sink).expect("spawn");
        let (mut reader, _writer, _child, _resizer) = session.split();

        let text = sink_until(&collected, "felis-sink").await;
        assert!(text.contains("felis-sink"), "sink missed output: {text:?}");

        // The Unix master reports EOF when the child exits; `ConPTY`
        // closes the output pipe only at `ClosePseudoConsole`, so an
        // idle timeout is a pass too. A byte count is the failure.
        let mut buf = [0u8; 4096];
        match tokio::time::timeout(Duration::from_secs(2), reader.read(&mut buf)).await {
            Ok(Ok(n)) => assert_eq!(n, 0, "the lifecycle reader yielded data"),
            Ok(Err(err)) => panic!("lifecycle reader errored: {err}"),
            Err(_) => {}
        }
    }

    /// The lifecycle EOF is ordered after the sink's last call. The sink
    /// sleeps long enough that a "signal EOF, then drain" reader loses
    /// the race every time. Unix-only: `ConPTY` never EOFs.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn eof_is_reported_only_after_the_sink_drained() {
        let collected = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink_side = Arc::clone(&collected);
        let session = spawn(
            shell_echo("felis-drain"),
            small_size(),
            Box::new(move |bytes| {
                thread::sleep(Duration::from_millis(250));
                sink_side
                    .lock()
                    .expect("sink collector lock")
                    .extend_from_slice(bytes);
            }),
        )
        .expect("spawn");
        let (mut reader, _writer, _child, _resizer) = session.split();

        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(Duration::from_secs(5), reader.read(&mut buf))
            .await
            .expect("reader did not reach EOF")
            .expect("reader errored");
        assert_eq!(n, 0, "the lifecycle reader yielded data");

        let text = collected_text(&collected);
        assert!(
            text.contains("felis-drain"),
            "EOF outran the sink's last call: {text:?}"
        );
    }

    /// A panicking sink must not strand the reader thread: a reader
    /// parked against the full swap buffer is released and the
    /// lifecycle `PtyReader` still reaches EOF. The child floods well
    /// past `READ_PENDING_CAP` so the parked state is reachable.
    /// Unix-only: `ConPTY` never EOFs.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn panicking_sink_releases_reader_and_reports_eof() {
        let mut cmd = Command::new("/bin/sh");
        // ~4 MiB of output: enough to fill READ_PENDING_CAP (1 MiB)
        // after the sink dies on its first bytes.
        cmd.args(["-c", "dd if=/dev/zero bs=65536 count=64 2>/dev/null"]);
        let session = spawn(
            cmd,
            small_size(),
            Box::new(|_bytes| panic!("sink dies on first swap (deliberate, test-only)")),
        )
        .expect("spawn");
        let (mut reader, _writer, _child, _resizer) = session.split();

        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(Duration::from_secs(10), reader.read(&mut buf))
            .await
            .expect("reader must reach EOF after the sink panics, not park forever")
            .expect("clean EOF");
        assert_eq!(n, 0, "the lifecycle reader yielded data");
    }

    /// The child's stdout reaches the sink across the PTY. On Windows a
    /// child misrouted to the parent's std handles (`STARTF_USESTDHANDLES`)
    /// surfaces here as an empty sink.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn echoes_short_string_back() {
        let (collected, sink) = collector();
        let session = spawn(shell_echo("felis-pty"), small_size(), sink).expect("spawn");
        let (_reader, _writer, child, _resizer) = session.split();

        let text = sink_until(&collected, "felis-pty").await;
        assert!(text.contains("felis-pty"), "expected echo, got: {text:?}");

        let mut waited = None;
        for _ in 0..50 {
            if let Some(status) = child.try_wait().expect("try_wait") {
                waited = Some(status);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(waited.is_some(), "child did not exit");
    }

    /// Cooked-mode echo is a Unix line-discipline contract; `ConPTY`
    /// input handling is not the same mechanism.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn write_round_trips_via_cat() {
        use tokio::io::AsyncWriteExt;

        let mut cmd = Command::new("/bin/sh");
        // `head -c N` exits on its own; `cat` would hang if the test raced.
        cmd.args(["-c", "head -c 5"]);
        cmd.env_clear();
        cmd.env("PATH", fixture_path());

        let (collected, sink) = collector();
        let session = spawn(cmd, small_size(), sink).expect("spawn");
        let (_reader, mut writer, _child, _resizer) = session.split();

        writer.write_all(b"hello").await.expect("write");
        writer.flush().await.expect("flush");

        let text = sink_until(&collected, "hello").await;
        assert!(text.contains("hello"), "input was not echoed: {text:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn killing_the_child_reaps_it() {
        let (_collected, sink) = collector();
        let session = spawn(blocks_forever(), small_size(), sink).expect("spawn");
        let (reader, writer, child, resizer) = session.split();
        drop(reader);
        drop(writer);
        drop(resizer);
        // Dropping the halves is not a teardown
        // (see `hangup_ends_a_child_that_outlives_the_master`).
        assert!(child.kill().expect("kill"), "a live child is signaled");

        let mut waited = None;
        for _ in 0..100 {
            if let Some(status) = child.try_wait().expect("try_wait") {
                waited = Some(status);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(waited.is_some(), "kill did not reap child");
        assert!(
            !child.kill().expect("kill"),
            "a reaped child must not be signaled again — that pid may be someone else's by now",
        );
    }

    #[cfg(unix)]
    async fn wait_for_exit(child: &ChildHandle) -> bool {
        for _ in 0..100 {
            if child.try_wait().expect("try_wait").is_some() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// The child is its own session leader (`setsid` + `TIOCSCTTY` in
    /// `pre_exec`), so the reported foreground pgid is its pid.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn foreground_pgrp_reports_a_live_child() {
        let mut cmd = Command::new("/bin/sh");
        // No API hands the caller the spawned pid, so the child reports
        // `$$`. Marker last so waiting on it also guarantees the digits
        // arrived.
        cmd.args(["-c", "printf '%d up\\n' $$; sleep 60"]);
        let (collected, sink) = collector();
        let session = spawn(cmd, small_size(), sink).expect("spawn");
        let (_reader, _writer, child, resizer) = session.split();
        // Wait for the marker, not just for `spawn` to return: the
        // `pre_exec` handshake that installs the foreground pgrp can
        // still be pending when the parent resumes.
        let seen = sink_until(&collected, "up").await;
        let reported: i32 = seen
            .split_whitespace()
            .next()
            .and_then(|pid| pid.parse().ok())
            .unwrap_or_else(|| panic!("child never reported its pid: {seen:?}"));

        assert_eq!(
            resizer.foreground_pgrp(),
            Some(reported),
            "the session leader's pgid is its own pid",
        );

        child.kill().expect("kill");
    }

    /// A terminal whose session leader has exited reports "nothing in
    /// the foreground", never panics (the `tcgetpgrp` why-not in
    /// `unix::Master::foreground_pgrp`).
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn foreground_pgrp_survives_a_dead_session_leader() {
        let (collected, sink) = collector();
        let session = spawn(shell_echo("gone"), small_size(), sink).expect("spawn");
        let (_reader, _writer, child, resizer) = session.split();
        sink_until(&collected, "gone").await;
        assert!(wait_for_exit(&child).await, "echo child never exited");

        // The platforms disagree on whether a reaped leader leaves a
        // stale pgid or clears it; a reported pgid must still be usable.
        if let Some(pgrp) = resizer.foreground_pgrp() {
            assert!(pgrp > 0, "a reported pgid must be positive, got {pgrp}");
        }
    }

    /// Every master half the caller owns can be gone and the child
    /// still runs; only a signal ends it.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hangup_ends_a_child_that_outlives_the_master() {
        let (_collected, sink) = collector();
        let session = spawn(blocks_forever(), small_size(), sink).expect("spawn");
        let (reader, writer, child, resizer) = session.split();
        drop(reader);
        drop(writer);
        drop(resizer);

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "dropping the master halves must be shown not to end the child",
        );

        assert!(child.hangup().expect("hangup"), "a live child is signaled");
        let mut waited = None;
        for _ in 0..100 {
            if let Some(status) = child.try_wait().expect("try_wait") {
                waited = Some(status);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(waited.is_some(), "the hung-up child never exited");
    }

    /// A sink that parks inside `write` until the test opens it, so the
    /// pending gauge is exercised against a controlled stall rather than
    /// a platform-dependent pipe buffer.
    #[derive(Clone)]
    struct GatedSink {
        inner: Arc<GateInner>,
    }

    struct GateInner {
        state: Mutex<GateState>,
        changed: Condvar,
    }

    struct GateState {
        open: bool,
        failing: bool,
        taken: usize,
    }

    impl GatedSink {
        fn new() -> Self {
            Self {
                inner: Arc::new(GateInner {
                    state: Mutex::new(GateState {
                        open: false,
                        failing: false,
                        taken: 0,
                    }),
                    changed: Condvar::new(),
                }),
            }
        }

        fn open(&self) {
            let mut state = self.inner.state.lock().unwrap();
            state.open = true;
            self.inner.changed.notify_all();
        }

        /// Re-gate a sink an earlier `open` released, so a test can hold
        /// a second batch of writes unwritten.
        fn close(&self) {
            let mut state = self.inner.state.lock().unwrap();
            state.open = false;
        }

        fn fail(&self) {
            let mut state = self.inner.state.lock().unwrap();
            state.failing = true;
            self.inner.changed.notify_all();
        }

        fn taken(&self) -> usize {
            self.inner.state.lock().unwrap().taken
        }
    }

    impl Write for GatedSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut state = self.inner.state.lock().unwrap();
            while !state.open && !state.failing {
                state = self.inner.changed.wait(state).unwrap();
            }
            if state.failing {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "sink closed"));
            }
            state.taken += buf.len();
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A child that stopped reading its stdin must park the writer, not
    /// grow the queue.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn writes_park_once_the_pending_cap_is_reached() {
        use tokio::io::AsyncWriteExt;

        let sink = GatedSink::new();
        let mut writer = spawn_writer(sink.clone()).expect("writer");

        writer
            .write_all(&vec![7u8; PTY_WRITE_PENDING_CAP])
            .await
            .expect("the first write fills the gauge without waiting");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), writer.write_all(b"x"))
                .await
                .is_err(),
            "a write past the cap must wait for the sink, not queue",
        );

        sink.open();
        tokio::time::timeout(Duration::from_secs(5), writer.write_all(b"x"))
            .await
            .expect("the drained sink frees space")
            .expect("write");
        tokio::time::timeout(Duration::from_secs(5), writer.flush())
            .await
            .expect("flush completes once the sink took everything")
            .expect("flush");
        assert_eq!(sink.taken(), PTY_WRITE_PENDING_CAP + 1);
    }

    /// "Flushed" must mean the OS writer took the bytes; the callers
    /// that report a write failure have nothing else to wait on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn flush_waits_for_the_sink_not_the_enqueue() {
        use tokio::io::AsyncWriteExt;

        let sink = GatedSink::new();
        let mut writer = spawn_writer(sink.clone()).expect("writer");
        writer.write_all(b"hello").await.expect("write");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), writer.flush())
                .await
                .is_err(),
            "flush must not complete while the bytes are still queued",
        );

        sink.open();
        tokio::time::timeout(Duration::from_secs(5), writer.flush())
            .await
            .expect("flush completes after delivery")
            .expect("flush");
        assert_eq!(sink.taken(), 5);
    }

    /// The failure belongs to the flush: the write that queued the bytes
    /// had nothing to report yet.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn flush_reports_a_sink_failure() {
        use tokio::io::AsyncWriteExt;

        let sink = GatedSink::new();
        let mut writer = spawn_writer(sink.clone()).expect("writer");
        writer.write_all(b"hello").await.expect("the write queues");
        sink.fail();

        let err = tokio::time::timeout(Duration::from_secs(5), writer.flush())
            .await
            .expect("flush resolves once the write failed")
            .expect_err("the sink failure surfaces");
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }

    /// Shutdown drains what is queued and then closes: a write after it
    /// must fail rather than queue bytes for a thread that is leaving.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_drains_then_refuses_further_writes() {
        use tokio::io::AsyncWriteExt;

        let sink = GatedSink::new();
        let mut writer = spawn_writer(sink.clone()).expect("writer");
        writer.write_all(b"hello").await.expect("write");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), writer.shutdown())
                .await
                .is_err(),
            "shutdown must not complete while the bytes are still queued",
        );

        sink.open();
        tokio::time::timeout(Duration::from_secs(5), writer.shutdown())
            .await
            .expect("shutdown completes after delivery")
            .expect("shutdown");
        assert_eq!(sink.taken(), 5);

        let err = writer
            .write_all(b"after")
            .await
            .expect_err("a write past shutdown has nowhere to go");
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(sink.taken(), 5, "the refused write reached no sink");
        assert_eq!(
            writer.pending_bytes(),
            0,
            "the refused write leaves no bytes a later flush would wait on",
        );
    }

    /// The drop notice is one per saturation episode, not one per
    /// dropped reply: a child that queries in a loop while refusing to
    /// read its stdin must not be able to write the daemon's log. A
    /// gauge that drains opens the next episode.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_drop_notice_is_claimed_once_per_saturation_episode() {
        let sink = GatedSink::new();
        let writer = spawn_writer(sink.clone()).expect("writer");

        assert!(
            writer
                .write_owned(vec![0u8; PTY_WRITE_PENDING_CAP], None)
                .expect("enqueue")
                .queued(),
            "an empty gauge takes the write whole",
        );
        let mut reported = 0;
        for _ in 0..3 {
            match writer.write_owned(b"reply".to_vec(), None).expect("gauge") {
                WriteOutcome::Dropped { report } => reported += usize::from(report),
                WriteOutcome::Queued => panic!("a full unreserved gauge takes nothing"),
            }
        }
        assert_eq!(
            reported, 1,
            "one episode reports once, however many replies it drops",
        );

        sink.open();
        for _ in 0..100 {
            if writer.pending_bytes() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(writer.pending_bytes(), 0, "the gauge drained");

        // Re-gated before the refill, so the second episode is reached
        // by the cap rather than by racing the writer thread.
        sink.close();
        assert!(
            writer
                .write_owned(vec![0u8; PTY_WRITE_PENDING_CAP], None)
                .expect("enqueue")
                .queued(),
            "the drained gauge takes a write again",
        );
        assert_eq!(
            writer.write_owned(b"reply".to_vec(), None).expect("gauge"),
            WriteOutcome::Dropped { report: true },
            "a gauge that emptied in between opens a new episode",
        );

        sink.open();
    }

    /// The daemon's non-blocking path: reserved bytes always enqueue,
    /// unreserved ones are dropped rather than queued past the cap.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn write_owned_drops_unreserved_bytes_past_the_cap() {
        let sink = GatedSink::new();
        let writer = spawn_writer(sink.clone()).expect("writer");

        assert!(
            writer
                .write_owned(vec![0u8; PTY_WRITE_PENDING_CAP], None)
                .expect("enqueue")
                .queued(),
            "an empty gauge takes the write whole",
        );
        assert!(
            !writer
                .write_owned(b"reply".to_vec(), None)
                .expect("gauge")
                .queued(),
            "an unreserved write past the cap is dropped",
        );
        let reservation: WriteReservation = Box::new(());
        assert!(
            writer
                .write_owned(b"typed".to_vec(), Some(reservation))
                .expect("enqueue")
                .queued(),
            "reserved bytes were admitted upstream and always enqueue",
        );
        assert_eq!(writer.pending_bytes(), PTY_WRITE_PENDING_CAP as u64 + 5);

        sink.open();
        for _ in 0..100 {
            if writer.pending_bytes() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(sink.taken(), PTY_WRITE_PENDING_CAP + 5);
    }

    /// A paste the daemon admitted against its own budget must not
    /// starve the session's replies: a child that is merely behind on a
    /// 16 MiB paste still has to receive its `DA` answer and the mouse
    /// reports of the window driving it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn admitted_bytes_do_not_close_the_unreserved_gauge() {
        let sink = GatedSink::new();
        let writer = spawn_writer(sink.clone()).expect("writer");

        let reservation: WriteReservation = Box::new(());
        assert!(
            writer
                .write_owned(vec![0u8; PTY_WRITE_PENDING_CAP * 4], Some(reservation))
                .expect("enqueue")
                .queued(),
            "reserved bytes always enqueue",
        );
        assert!(
            writer
                .write_owned(b"reply".to_vec(), None)
                .expect("gauge")
                .queued(),
            "an unreserved reply behind a reserved paste still enqueues",
        );

        sink.open();
        for _ in 0..100 {
            if writer.pending_bytes() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(sink.taken(), PTY_WRITE_PENDING_CAP * 4 + 5);
    }

    fn test_reader() -> (Arc<Mutex<LifecycleState>>, PtyReader) {
        let lifecycle = Arc::new(Mutex::new(LifecycleState {
            eof: false,
            err: None,
            waker: None,
        }));
        let placeholder = thread::spawn(|| {});
        let reader = PtyReader {
            lifecycle: Arc::clone(&lifecycle),
            _reader_thread: placeholder,
        };
        (lifecycle, reader)
    }

    /// A read error reaches the consumer once, then a clean EOF.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_error_surfaces_once_then_clean_eof() {
        let (lifecycle, mut reader) = test_reader();
        {
            let mut state = lifecycle.lock().unwrap();
            state.eof = true;
            state.err = Some(io::Error::other("pty gone"));
        }

        let mut buf = [0u8; 8];
        assert!(reader.read(&mut buf).await.is_err(), "error first");
        assert_eq!(reader.read(&mut buf).await.expect("clean EOF"), 0);
    }

    #[cfg(unix)]
    fn cat() -> Command {
        Command::new("cat")
    }

    /// A child that reads no input, in raw mode so the tty queues input
    /// instead of discarding past a canonical line limit.
    #[cfg(unix)]
    fn raw_non_reader() -> Command {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "stty raw -echo; printf ready; exec sleep 1000"]);
        cmd
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn park_completes_on_an_idle_session_and_resume_keeps_it_working() {
        let (collected, sink) = collector();
        let session = spawn(cat(), small_size(), sink).expect("spawn");
        let quiescer = session.quiescer();
        let (_reader, mut writer, child, _resizer) = session.split();

        let parked = quiescer
            .park(Duration::from_secs(2))
            .expect("park an idle session");
        assert_eq!(parked, Parked::default(), "an idle session holds no input");
        quiescer.resume().expect("resume");

        tokio::io::AsyncWriteExt::write_all(&mut writer, b"after-resume\n")
            .await
            .expect("write");
        assert!(
            sink_until(&collected, "after-resume")
                .await
                .contains("after-resume")
        );
        child.kill().expect("kill");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn park_hands_back_input_a_non_reading_child_has_not_taken() {
        let (collected, sink) = collector();
        let session = spawn(raw_non_reader(), small_size(), sink).expect("spawn");
        let quiescer = session.quiescer();
        let (_reader, writer, child, _resizer) = session.split();
        assert!(sink_until(&collected, "ready").await.contains("ready"));

        let payload: Vec<u8> = (0..64 * 1024).map(|i| b'a' + (i % 26) as u8).collect();
        assert!(
            writer
                .write_owned(payload.clone(), None)
                .expect("queue")
                .queued()
        );
        // The tty queue fills well short of 64 KiB, leaving the writer
        // blocked on a child that never reads.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(writer.pending_bytes() > 0, "the writer is blocked");

        let parked = quiescer
            .park(Duration::from_secs(2))
            .expect("park a blocked writer");
        assert_ne!(parked.unwritten, Vec::<u8>::new());
        assert!(
            parked.unwritten.len() < payload.len(),
            "the tty took a prefix"
        );
        assert!(
            payload.ends_with(&parked.unwritten),
            "the unwritten bytes are exactly the payload's untaken tail",
        );

        let more = b"queued while parked".to_vec();
        assert!(
            writer
                .write_owned(more.clone(), None)
                .expect("queue")
                .queued()
        );
        let again = quiescer.park(Duration::from_secs(2)).expect("park again");
        assert_eq!(
            again.unwritten,
            [parked.unwritten.as_slice(), more.as_slice()].concat(),
            "input queued while parked is handed back after what was already held",
        );

        quiescer.resume().expect("resume");
        child.kill().expect("kill");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_adopted_session_reaches_the_same_child_through_the_same_master() {
        let (_collected, sink) = collector();
        let session = spawn(cat(), small_size(), sink).expect("spawn");
        let quiescer = session.quiescer();
        let (_reader, _writer, child, resizer) = session.split();
        quiescer.park(Duration::from_secs(2)).expect("park");

        let master = resizer
            .master_fd()
            .try_clone_to_owned()
            .expect("dup master");
        let pid = child.process_id().expect("pid").expect("live child");
        let (adopted_collected, adopted_sink) = collector();
        let adopted = adopt(master, pid, None, adopted_sink).expect("adopt");
        let (_reader, mut writer, adopted_child, _resizer) = adopted.split();

        tokio::io::AsyncWriteExt::write_all(&mut writer, b"through-adoption\n")
            .await
            .expect("write");
        assert!(
            sink_until(&adopted_collected, "through-adoption")
                .await
                .contains("through-adoption")
        );
        assert!(
            adopted_child.kill().expect("kill"),
            "the adopted child is live"
        );
        assert!(
            wait_for_exit(&adopted_child).await,
            "the adopted child is reaped by pid"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_adopted_child_already_reaped_is_never_signaled() {
        let (_collected, sink) = collector();
        let session = spawn(cat(), small_size(), sink).expect("spawn");
        let (_reader, _writer, child, resizer) = session.split();
        let master = resizer
            .master_fd()
            .try_clone_to_owned()
            .expect("dup master");

        // This test process's own pid: a signal sent to it would end the test run.
        let own_pid = i32::try_from(std::process::id()).expect("pid fits i32");
        let (_adopted_collected, adopted_sink) = collector();
        let adopted = adopt(master, own_pid, Some(0), adopted_sink).expect("adopt");
        let (_reader, _writer, adopted_child, _resizer) = adopted.split();

        assert!(
            !adopted_child.hangup().expect("hangup"),
            "nothing is sent to a reaped pid"
        );
        assert!(
            !adopted_child.kill().expect("kill"),
            "nothing is sent to a reaped pid"
        );
        assert!(adopted_child.try_wait().expect("try_wait").is_some());
        child.kill().expect("kill");
    }
}

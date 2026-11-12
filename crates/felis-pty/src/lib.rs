#![doc = include_str!("../README.md")]

use std::{
    io::{self, Read, Write},
    pin::Pin,
    sync::{Arc, Condvar, Mutex},
    task::{Context, Poll, Waker},
    thread::{self, JoinHandle},
};

use thiserror::Error;
use tokio::{
    io::AsyncWrite,
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
};

mod command;
pub mod handover;
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
#[cfg(windows)]
pub use windows::set_conpty_dir;

/// Pending-byte ceiling for the reader ⇄ parser handoff: the reader
/// parks once this many unparsed bytes are buffered. A flood amortizes
/// the park/wake pair over ~a megabyte; per-session memory is bounded
/// at roughly twice this (the two swap buffers' high-water marks).
const READ_PENDING_CAP: usize = 1024 * 1024;

/// Ceiling on unreserved bytes handed to [`PtyWriter`] that the OS has
/// not taken yet: a child that stops reading its input cannot grow the
/// queue without bound. At the ceiling, [`AsyncWrite`] writes wait and
/// [`PtyWriter::write_owned`] drops unreserved bytes. Reserved bytes
/// always queue, and one write admitted below the ceiling may pass it.
pub const PTY_WRITE_PENDING_CAP: usize = 1024 * 1024;

/// How often [`ChildHandle::wait_timeout`] looks for the child's exit.
const CHILD_WAIT_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// Maximum bytes per `read(2)` against the PTY master. 64 KiB matches
/// the kernel's pipe buffer ceiling on Linux and macOS, so a long `cat`
/// completes in a handful of reads.
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

/// A running session's handles, one per concern so each can move to
/// the task that owns it.
pub struct PtyParts {
    pub eof: PtyEof,
    pub writer: PtyWriter,
    pub child: ChildHandle,
    pub resizer: Resizer,
    pub quiescer: Quiescer,
}

/// Dropping this never signals the child. On Unix neither does dropping
/// the [`Resizer`], since the reader thread holds its own dup of the
/// master; on Windows that closes the pseudoconsole. A teardown that
/// wants the child gone says so through [`hangup`](ChildHandle::hangup)
/// or [`kill`](ChildHandle::kill).
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

    /// Reaps the child if it has exited. Nothing reaps it otherwise, so
    /// an owner that never calls this leaves a zombie on Unix.
    pub fn try_wait(&self) -> io::Result<Option<std::process::ExitStatus>> {
        let mut guard = self.child.lock().map_err(poison)?;
        guard.try_wait()
    }

    /// Reaps the child once it exits, or gives up at `timeout` with
    /// `None`. Polls [`try_wait`](Self::try_wait), and looks once more
    /// at the deadline.
    pub async fn wait_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> io::Result<Option<std::process::ExitStatus>> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(Some(status));
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(CHILD_WAIT_POLL.min(deadline - now)).await;
        }
    }
}

/// The terminal side of the session: its size and what it can report.
/// On Windows it owns the pseudoconsole, and dropping it closes that,
/// which ends the child's console.
pub struct Resizer {
    master: platform::Master,
    #[cfg(unix)]
    quiescer: Quiescer,
}

impl Resizer {
    pub fn resize(&self, size: Size) -> io::Result<()> {
        self.master.resize(size)
    }

    /// The pgid the kernel routes terminal input to (`tcgetpgrp` on the
    /// master). `None` when the platform cannot report it (Windows) or
    /// nothing holds the foreground.
    #[must_use]
    pub fn foreground_pgrp(&self) -> Option<i32> {
        self.master.foreground_pgrp()
    }

    /// Whether every byte the terminal side wrote before this call has
    /// been through the sink, whoever still holds that side open. Always
    /// `true` on Windows, whose `ConPTY` output cannot be told apart from
    /// what its renderer has yet to emit.
    #[must_use]
    #[cfg_attr(windows, allow(clippy::unused_self, clippy::missing_const_for_fn))]
    pub fn output_drained(&self) -> bool {
        #[cfg(unix)]
        {
            // The queue first: an empty one with the reader idle after it
            // means nothing written before the probe is still in flight.
            // At EOF the queue reads as readable on macOS, and is moot.
            let queued = self.master.output_queued();
            match self.quiescer.caught_up() {
                quiesce::CaughtUp::AtEof => true,
                quiesce::CaughtUp::Idle => !queued,
                quiesce::CaughtUp::No => false,
            }
        }
        #[cfg(windows)]
        {
            true
        }
    }
}

/// The reader thread's end-of-stream report to the async
/// [`PtyEof`]. No bytes cross here; the [`ByteSink`] owns those.
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

/// The end of the session's output: the child has closed its side of
/// the terminal and the [`ByteSink`] has taken every byte it wrote.
pub struct PtyEof {
    lifecycle: Arc<Mutex<LifecycleState>>,
    /// Never joined: it blocks in a kernel `read` until the far side of
    /// the terminal closes, and dropping this must not wait on that.
    _reader_thread: JoinHandle<()>,
}

impl PtyEof {
    /// Resolves after the sink's last call, first with the error if a
    /// read of the terminal failed. Cancel-safe. Each call spends Tokio's
    /// cooperative budget, so a `select!` arm left armed past the end
    /// still yields to the tasks beside it.
    pub async fn eof(&mut self) -> io::Result<()> {
        std::future::poll_fn(|cx| self.poll_eof(cx)).await
    }

    /// The poll form of [`eof`](Self::eof).
    #[expect(
        clippy::needless_pass_by_ref_mut,
        reason = "one waker slot: a second concurrent waiter would steal the first's wakeup"
    )]
    pub fn poll_eof(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
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
/// once the OS writer has taken them, so an input budget the caller
/// keeps covers the whole path, not just the enqueue. Reserved bytes are
/// exempt from [`PTY_WRITE_PENDING_CAP`].
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

/// The child's input. Bytes queue to a dedicated writer thread, so a
/// child that stops reading never blocks the caller; unreserved bytes in
/// the queue are bounded by [`PTY_WRITE_PENDING_CAP`].
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
/// on a dedicated parse thread. [`PtyEof`] resolves after the sink's
/// last call.
// By value: consuming the `Command` keeps a caller from re-spawning a
// stale env snapshot by accident.
#[allow(clippy::needless_pass_by_value)]
pub fn spawn(command: Command, size: Size, sink: ByteSink) -> Result<PtyParts, PtyError> {
    let (master, child) = platform::open_and_spawn(&command, size)?;
    session_over(master, child, sink)
}

fn session_over(
    master: platform::Master,
    child: platform::Child,
    sink: ByteSink,
) -> Result<PtyParts, PtyError> {
    let (eof, writer, quiescer) = spawn_io_threads(&master, sink)?;
    Ok(PtyParts {
        eof,
        writer,
        child: ChildHandle {
            child: Mutex::new(child),
        },
        resizer: Resizer {
            master,
            #[cfg(unix)]
            quiescer: quiescer.clone(),
        },
        quiescer,
    })
}

fn spawn_io_threads(
    master: &platform::Master,
    sink: ByteSink,
) -> Result<(PtyEof, PtyWriter, Quiescer), PtyError> {
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
    #[cfg(windows)]
    quiesce.set_reader_thread(
        std::os::windows::io::AsHandle::as_handle(&reader_handle)
            .try_clone_to_owned()
            .map_err(|e| PtyError::OpenPty(format!("duplicate the reader thread: {e}")))?,
    );

    let writer = spawn_writer_with(sync_writer, Arc::clone(&quiesce))?;
    let quiescer = Quiescer {
        shared: quiesce,
        sink: sink_shared,
        writer: Arc::clone(&writer.state),
    };
    Ok((
        PtyEof {
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
                    // leave `PtyEof` pending forever.
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
mod test_support;
#[cfg(test)]
mod tests;

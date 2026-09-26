use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
use tokio::sync::mpsc;

use super::{LockOrPoisoned as _, MAX_IN_FLIGHT, OUTPUT_ITEM_CAP};

/// One line read from the bridge's stdin, or the refusal of one that
/// was too long to hold.
pub(super) enum BridgeLine {
    Text(String),
    /// Answered uncorrelated (`id: null`): reading an id out of an
    /// over-limit line means parsing the very payload being refused.
    TooLong(felis_protocol::convert::WireError),
}

/// Reads stdin one request line at a time with bounded memory allocation.
///
/// Drops content past `cap` to prevent unbounded memory growth on long lines
/// without waiting for a newline (REQ-105a).
pub(super) struct BridgeLines<R> {
    pub(super) reader: R,
    pub(super) cap: usize,
    /// Bytes of the current line so far, newline excluded: counted
    /// even past `cap`, so a refusal can name the real length.
    pub(super) len: usize,
    /// The current line, dropped as soon as it passes `cap`.
    pub(super) buf: Vec<u8>,
    /// Distinguishes "nothing read yet" from an empty line.
    pub(super) started: bool,
}

impl<R: tokio::io::AsyncBufRead + Unpin> BridgeLines<R> {
    pub(super) const fn new(reader: R) -> Self {
        Self {
            reader,
            cap: felis_protocol::messages::MAX_BRIDGE_LINE_BYTES,
            len: 0,
            buf: Vec::new(),
            started: false,
        }
    }

    /// Read the next line, or `Ok(None)` at EOF.
    ///
    /// Discards over-limit lines until newline so subsequent lines parse.
    /// Returns `InvalidData` for non-UTF-8 input, matching `BufReader::lines`.
    pub(super) async fn next(&mut self) -> std::io::Result<Option<BridgeLine>> {
        loop {
            let (take, keep, done) = {
                let chunk = self.reader.fill_buf().await?;
                if chunk.is_empty() {
                    break;
                }
                let (take, keep, done) = match chunk.iter().position(|byte| *byte == b'\n') {
                    Some(at) => (at + 1, at, true),
                    None => (chunk.len(), chunk.len(), false),
                };
                // Saturating: the counter keeps running past the cap
                // so the whole over-limit line is measured, and a
                // newline-free stream must not wrap it back under.
                if self.len.saturating_add(keep) <= self.cap {
                    self.buf.extend_from_slice(&chunk[..keep]);
                } else {
                    self.buf = Vec::new();
                }
                (take, keep, done)
            };
            self.len = self.len.saturating_add(keep);
            self.started = true;
            self.reader.consume(take);
            if done {
                return self.take_line().map(Some);
            }
        }
        if self.started {
            return self.take_line().map(Some);
        }
        Ok(None)
    }

    fn take_line(&mut self) -> std::io::Result<BridgeLine> {
        let len = std::mem::replace(&mut self.len, 0);
        let mut buf = std::mem::take(&mut self.buf);
        self.started = false;
        if let Err(err) = felis_protocol::messages::check_limit("bridge line", len, self.cap) {
            return Ok(BridgeLine::TooLong(err));
        }
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
        String::from_utf8(buf)
            .map(BridgeLine::Text)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
    }
}

/// The single stdout writer: one task owns the pipe so concurrent
/// operations cannot interleave halves of two objects on one line.
#[derive(Clone)]
pub(super) struct Out {
    inner: Arc<OutInner>,
}

struct OutInner {
    tx: mpsc::Sender<OutLine>,
    item_budget: Arc<tokio::sync::Semaphore>,
    terminal_budget: Arc<tokio::sync::Semaphore>,
    failure: std::sync::Mutex<Option<String>>,
    failed: tokio::sync::Notify,
    /// Held while a delimiter is handed to stdout and its `written`
    /// callback runs, and by every id lookup, so a client that has read
    /// the delimiter can never find its id still held.
    publishing: std::sync::Mutex<()>,
}

pub(super) enum OutLine {
    Data {
        text: String,
        _item: Option<tokio::sync::OwnedSemaphorePermit>,
        terminal: Option<TerminalDelivery>,
        written: Option<Box<dyn FnOnce() + Send>>,
    },
    Close,
}

pub(super) struct TerminalDelivery {
    _budget: tokio::sync::OwnedSemaphorePermit,
}

pub(super) struct TerminalSlot {
    queue: mpsc::OwnedPermit<OutLine>,
    budget: tokio::sync::OwnedSemaphorePermit,
}

impl TerminalSlot {
    pub(super) fn send(self, text: String, written: impl FnOnce() + Send + 'static) {
        let Self { queue, budget } = self;
        let _sender = queue.send(OutLine::Data {
            text,
            _item: None,
            terminal: Some(TerminalDelivery { _budget: budget }),
            written: Some(Box::new(written)),
        });
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct OutputClosed;

impl Out {
    pub(super) fn start() -> (Self, tokio::task::JoinHandle<()>) {
        Self::start_with_writer(tokio::io::stdout())
    }

    pub(super) fn start_with_writer<W>(mut writer: W) -> (Self, tokio::task::JoinHandle<()>)
    where
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (tx, mut rx) = mpsc::channel(OUTPUT_ITEM_CAP + MAX_IN_FLIGHT);
        let out = Self {
            inner: Arc::new(OutInner {
                tx,
                item_budget: Arc::new(tokio::sync::Semaphore::new(OUTPUT_ITEM_CAP)),
                terminal_budget: Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT)),
                failure: std::sync::Mutex::new(None),
                failed: tokio::sync::Notify::new(),
                publishing: std::sync::Mutex::new(()),
            }),
        };
        let reporter = out.clone();
        let task = tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                let OutLine::Data {
                    text,
                    _item,
                    terminal,
                    mut written,
                } = message
                else {
                    break;
                };
                if let Err(err) = writer.write_all(text.as_bytes()).await {
                    *reporter.inner.failure.lock_or_poisoned() = Some(err.to_string());
                    reporter.inner.failed.notify_waiters();
                    tracing::warn!(%err, "bridge: stdout failed; shutting down");
                    break;
                }
                // Not `write_all` then `written()`: tokio's stdout hands the
                // byte to a blocking thread, so the client can read the
                // delimiter and reuse the id before `write_all` returns.
                let result = std::future::poll_fn(|cx| {
                    let _publishing = reporter.inner.publishing.lock_or_poisoned();
                    match std::task::ready!(tokio::io::AsyncWrite::poll_write(
                        std::pin::Pin::new(&mut writer),
                        cx,
                        b"\n"
                    )) {
                        Ok(0) => std::task::Poll::Ready(Err(std::io::ErrorKind::WriteZero.into())),
                        Ok(_) => {
                            if let Some(written) = written.take() {
                                written();
                            }
                            std::task::Poll::Ready(Ok(()))
                        }
                        Err(err) => std::task::Poll::Ready(Err(err)),
                    }
                })
                .await;
                let result = match result {
                    Ok(()) => writer.flush().await,
                    Err(err) => Err(err),
                };
                if let Err(err) = result {
                    *reporter.inner.failure.lock_or_poisoned() = Some(err.to_string());
                    reporter.inner.failed.notify_waiters();
                    tracing::warn!(%err, "bridge: stdout failed; shutting down");
                    break;
                }
                drop(terminal);
            }
        });
        (out, task)
    }

    pub(super) fn publication(&self) -> std::sync::MutexGuard<'_, ()> {
        self.inner.publishing.lock_or_poisoned()
    }

    pub(super) async fn reserve_terminal(&self) -> Result<TerminalSlot, OutputClosed> {
        let budget = Arc::clone(&self.inner.terminal_budget)
            .acquire_owned()
            .await
            .map_err(|_| OutputClosed)?;
        let queue = self
            .inner
            .tx
            .clone()
            .reserve_owned()
            .await
            .map_err(|_| OutputClosed)?;
        Ok(TerminalSlot { queue, budget })
    }

    pub(super) async fn reserve_item(
        &self,
    ) -> Result<
        (
            mpsc::OwnedPermit<OutLine>,
            tokio::sync::OwnedSemaphorePermit,
        ),
        OutputClosed,
    > {
        let budget = Arc::clone(&self.inner.item_budget)
            .acquire_owned()
            .await
            .map_err(|_| OutputClosed)?;
        let queue = self
            .inner
            .tx
            .clone()
            .reserve_owned()
            .await
            .map_err(|_| OutputClosed)?;
        Ok((queue, budget))
    }

    pub(super) async fn emit(&self, text: String) -> Result<(), OutputClosed> {
        self.emit_with_written(text, || {}).await
    }

    pub(super) async fn emit_with_written(
        &self,
        text: String,
        written: impl FnOnce() + Send + 'static,
    ) -> Result<(), OutputClosed> {
        let (queue, budget) = self.reserve_item().await?;
        let _sender = queue.send(OutLine::Data {
            text,
            _item: Some(budget),
            terminal: None,
            written: Some(Box::new(written)),
        });
        Ok(())
    }

    pub(super) async fn close(&self) -> Result<(), OutputClosed> {
        self.inner
            .tx
            .send(OutLine::Close)
            .await
            .map_err(|_| OutputClosed)
    }

    pub(super) async fn failed(&self) -> String {
        loop {
            let notified = self.inner.failed.notified();
            let failure = self.inner.failure.lock_or_poisoned().clone();
            if let Some(detail) = failure {
                return detail;
            }
            notified.await;
        }
    }

    pub(super) fn failure_detail(&self) -> String {
        self.inner
            .failure
            .lock_or_poisoned()
            .clone()
            .unwrap_or_else(|| "the stdout writer stopped".to_owned())
    }

    pub(super) fn has_failed(&self) -> bool {
        self.inner.failure.lock_or_poisoned().is_some()
    }
}

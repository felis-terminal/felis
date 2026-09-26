use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use felis_client_core::{AttachIntent, CarrierConnection, Offer, Reconnector};
use felis_protocol::messages::StreamId;
use serde_json::Value;

use super::{
    LockOrPoisoned as _, MAX_AUXILIARY_LINKS, MAX_IN_FLIGHT, SHUTDOWN_GRACE,
    admission::{DaemonOp, Operation, Params, Request, cancel_target, parse_request},
    envelope::{Body, BridgeError, envelope, error_object, error_terminal_object, failure_object},
    link::{Link, dial, discard, drain_rehydrate},
    stdio::{Out, OutLine, OutputClosed, TerminalSlot},
};
use crate::cli_output::ErrorKind;

pub(super) struct ActiveOp {
    pub(super) state: std::sync::Mutex<ActiveState>,
    pub(super) id: Value,
    pub(super) stream: std::sync::Mutex<StreamSlot>,
    pub(super) task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// A point request reports its shutdown failure as an `error`
    /// reply, a stream as an `error` terminal.
    pub(super) streaming: bool,
}

pub(super) struct ActiveState {
    pub(super) done: bool,
    pub(super) terminal: Option<TerminalSlot>,
}

#[derive(Default)]
pub(super) struct StreamSlot {
    link: Option<Arc<Link>>,
    stream_id: Option<StreamId>,
    /// A cancel that arrived while `bound` was still empty. The window
    /// is not a hairline: `sessions.capture` spans a roster round trip
    /// and a full dial, attach, and rehydrate drain before it opens its
    /// stream.
    pub(super) canceled: bool,
}

impl StreamSlot {
    pub(super) fn cancel(&mut self) {
        if self.canceled {
            return;
        }
        self.canceled = true;
        if let (Some(link), Some(id)) = (self.link.clone(), self.stream_id) {
            link.cancel(id);
        }
    }
}

impl ActiveOp {
    pub(super) async fn emit_open(&self, out: &Out, object: String) -> Result<(), OutputClosed> {
        let (queue, budget) = out.reserve_item().await?;
        let state = self.state.lock_or_poisoned();
        if !state.done {
            let _sender = queue.send(OutLine::Data {
                text: object,
                _item: Some(budget),
                terminal: None,
                written: None,
            });
        }
        Ok(())
    }

    /// `false` means the operation already had a terminal and this one
    /// is discarded, which keeps "exactly one terminal per stream" true
    /// when a natural end and the shutdown path race.
    fn terminate(&self, object: String, written: impl FnOnce() + Send + 'static) -> bool {
        let mut state = self.state.lock_or_poisoned();
        if state.done {
            return false;
        }
        let Some(terminal) = state.terminal.take() else {
            return false;
        };
        state.done = true;
        terminal.send(object, written);
        true
    }

    fn shutdown_terminal(&self, err: &BridgeError) -> String {
        failure_object(&self.id, self.streaming, err)
    }
}

pub(super) struct Core {
    pub(super) target: Reconnector,
    pub(super) out: Out,
    /// Its death is the daemon's death as far as the bridge is
    /// concerned.
    pub(super) anchor: Arc<Link>,
    /// Per attach target, never per request: `capture` and `search` on
    /// one session interleave their streams over one connection. Closed
    /// by the last user because an attached connection is a live
    /// subscriber.
    pub(super) sessions: tokio::sync::Mutex<HashMap<u128, SessionSlot>>,
    /// Keyed by the rendered id; an entry is dropped as its terminal
    /// is published to stdout, so the table matches the client's pending set.
    pub(super) active: std::sync::Mutex<HashMap<String, Arc<ActiveOp>>>,
    /// Immediate refusals and control replies do not consume operation
    /// admission, but still keep their id until stdout publishes them.
    pub(super) pending_replies: std::sync::Mutex<HashMap<String, usize>>,
    pub(super) links: Arc<tokio::sync::Semaphore>,
}

pub(super) struct SessionSlot {
    link_permit: tokio::sync::OwnedSemaphorePermit,
    /// *Replaced* rather than re-counted when a dead link is redialed.
    pub(super) link: Option<Arc<Link>>,
    /// Counted when the slot is taken, before the link exists, so a
    /// replacement link never inherits a count that does not describe
    /// it (a user of a dead link releasing the live one out from under
    /// its own users).
    pub(super) users: usize,
}

impl SessionSlot {
    pub(super) const fn vacant(link_permit: tokio::sync::OwnedSemaphorePermit) -> Self {
        Self {
            link_permit,
            link: None,
            users: 0,
        }
    }

    fn live(&self) -> Option<Arc<Link>> {
        self.link.clone().filter(|link| !link.is_lost())
    }
}

impl Core {
    /// Never blocks on the daemon: each request runs in its own task,
    /// so a `capture` that streams for a minute cannot delay the `list`
    /// typed after it. Waiting for stdout capacity deliberately pauses
    /// stdin admission instead of growing another queue.
    pub(super) async fn accept(self: &Arc<Self>, line: &str) -> Result<(), OutputClosed> {
        if line.trim().is_empty() {
            return Ok(());
        }
        let request = match parse_request(line) {
            Ok(request) => request,
            Err((id, streaming, err)) => {
                // A terminal echoes the id it terminates; with no usable
                // id the only shape left is the untagged error, the one
                // shape the published schemas let answer without one.
                let object = failure_object(&id, streaming && !id.is_null(), &err);
                self.answer_immediate(&id, object).await?;
                return Ok(());
            }
        };
        let key = request.id.to_string();
        if self.id_in_use(&key) {
            self.answer_immediate(
                &request.id,
                error_object(
                    &request.id,
                    &BridgeError::malformed("id is already in flight on this bridge"),
                ),
            )
            .await?;
            return Ok(());
        }
        let operation = match self.validate_request(&request) {
            Ok(Operation::Daemon(operation)) => operation,
            Ok(Operation::Cancel) => {
                self.answer_immediate(&request.id, self.cancel(&request))
                    .await?;
                return Ok(());
            }
            Err(err) => {
                let object = failure_object(&request.id, request.streaming(), &err);
                self.answer_immediate(&request.id, object).await?;
                return Ok(());
            }
        };
        if self.active.lock_or_poisoned().len() >= MAX_IN_FLIGHT {
            let err = BridgeError::new(
                ErrorKind::AtCapacity,
                format!("the bridge already has {MAX_IN_FLIGHT} operations in flight"),
            );
            let object = failure_object(&request.id, operation.is_streaming(), &err);
            self.answer_immediate(&request.id, object).await?;
            return Ok(());
        }
        let terminal = self.out.reserve_terminal().await?;
        let entry = Arc::new(ActiveOp {
            state: std::sync::Mutex::new(ActiveState {
                done: false,
                terminal: Some(terminal),
            }),
            id: request.id.clone(),
            stream: std::sync::Mutex::new(StreamSlot::default()),
            task: std::sync::Mutex::new(None),
            streaming: operation.is_streaming(),
        });
        let op = Op {
            out: self.out.clone(),
            core: Arc::clone(self),
            key: key.clone(),
            entry: Arc::clone(&entry),
        };
        let _replaced = self
            .active
            .lock_or_poisoned()
            .insert(key, Arc::clone(&entry));
        let core = Arc::clone(self);
        let task = tokio::spawn(async move {
            let outcome = core.dispatch(&op, operation, &request).await;
            if let Err(err) = outcome {
                if op.streaming() {
                    op.error_terminal(&err);
                } else {
                    op.error(&err);
                }
            }
        });
        *entry.task.lock_or_poisoned() = Some(task);
        Ok(())
    }

    pub(super) fn id_in_use(&self, key: &str) -> bool {
        let _publication = self.out.publication();
        self.active.lock_or_poisoned().contains_key(key)
            || self.pending_replies.lock_or_poisoned().contains_key(key)
    }

    pub(super) async fn answer_immediate(
        self: &Arc<Self>,
        id: &Value,
        object: String,
    ) -> Result<(), OutputClosed> {
        if id.is_null() {
            return self.out.emit(object).await;
        }
        let key = id.to_string();
        *self
            .pending_replies
            .lock_or_poisoned()
            .entry(key.clone())
            .or_insert(0) += 1;
        let core = Arc::downgrade(self);
        let written_key = key.clone();
        if let Err(err) = self
            .out
            .emit_with_written(object, move || {
                if let Some(core) = core.upgrade() {
                    core.retire_immediate(&written_key);
                }
            })
            .await
        {
            self.retire_immediate(&key);
            return Err(err);
        }
        Ok(())
    }

    pub(super) async fn answer_after_loss(
        self: &Arc<Self>,
        line: &str,
        loss: &BridgeError,
    ) -> Result<(), OutputClosed> {
        let request = match parse_request(line) {
            Ok(request) => request,
            Err((id, streaming, err)) => {
                let object = failure_object(&id, streaming && !id.is_null(), &err);
                return self.answer_immediate(&id, object).await;
            }
        };
        if self.id_in_use(&request.id.to_string()) {
            return self
                .answer_immediate(
                    &request.id,
                    error_object(
                        &request.id,
                        &BridgeError::malformed("id is already in flight on this bridge"),
                    ),
                )
                .await;
        }
        if request.op == Ok(Operation::Cancel) {
            return self
                .answer_immediate(&request.id, self.cancel(&request))
                .await;
        }
        let streaming = request.streaming();
        let object = match self.validate_request(&request) {
            Ok(_) => failure_object(&request.id, streaming, loss),
            Err(err) => failure_object(&request.id, streaming, &err),
        };
        self.answer_immediate(&request.id, object).await
    }

    fn retire_immediate(&self, key: &str) {
        let mut pending = self.pending_replies.lock_or_poisoned();
        let Some(count) = pending.get_mut(key) else {
            return;
        };
        *count -= 1;
        if *count == 0 {
            pending.remove(key);
        }
    }

    /// An id is released as its terminal is published, so by the time the
    /// task unwinds a new request may already hold it.
    fn retire(&self, key: &str, entry: &Arc<ActiveOp>) {
        let mut active = self.active.lock_or_poisoned();
        if active.get(key).is_some_and(|live| Arc::ptr_eq(live, entry)) {
            let _retired = active.remove(key);
        }
    }

    /// The daemon's own terminal still closes the stream (a cancel
    /// races it in both directions), so `canceled` reports that the
    /// target was found and asked to stop, not that it had stopped.
    fn cancel(&self, request: &Request) -> String {
        let params = Params(&request.params);
        if let Err(err) = params.only(Operation::Cancel.params()) {
            return error_object(&request.id, &err);
        }
        let Some(target) = cancel_target(&params) else {
            return error_object(
                &request.id,
                &BridgeError::malformed("`target` must be the id of an in-flight request"),
            );
        };
        let found = self.active.lock_or_poisoned().get(&target).cloned();
        let canceled = match found {
            Some(entry) => {
                entry.stream.lock_or_poisoned().cancel();
                true
            }
            None => false,
        };
        envelope(
            &request.id,
            None,
            Body::Result(serde_json::json!({ "canceled": canceled })),
        )
    }

    async fn dispatch(
        &self,
        op: &Op,
        operation: DaemonOp,
        request: &Request,
    ) -> Result<(), BridgeError> {
        self.validate_request(request)?;
        let params = Params(&request.params);
        match operation {
            DaemonOp::List => self.op_list(op, &params).await,
            DaemonOp::Info => self.op_info(op, &params).await,
            DaemonOp::Spawn => self.op_spawn(op, &params).await,
            DaemonOp::Kill => self.op_kill(op, &params).await,
            DaemonOp::Evict => self.op_evict(op, &params).await,
            DaemonOp::Switch => self.op_switch(op, &params).await,
            DaemonOp::Tag => self.op_tag(op, &params).await,
            DaemonOp::Send => self.op_send(op, &params).await,
            DaemonOp::Capture => self.op_capture(op, &params).await,
            DaemonOp::Search => self.op_search(op, &params).await,
            DaemonOp::Subscribe => self.op_notifications(op, &params).await,
        }
    }

    pub(super) fn reserve_link(&self) -> Result<tokio::sync::OwnedSemaphorePermit, BridgeError> {
        Arc::clone(&self.links).try_acquire_owned().map_err(|_| {
            BridgeError::new(
                ErrorKind::AtCapacity,
                format!("the bridge already holds {MAX_AUXILIARY_LINKS} auxiliary daemon links"),
            )
        })
    }

    /// Borrow the attached link for the session a prefix names.
    ///
    /// The attach carries the prefix, so a second match appearing
    /// mid-flight is answered `ambiguous`; resolving separately would
    /// attach by an id picked before that session existed.
    async fn acquire_by_prefix(&self, prefix: &str) -> Result<(u128, Arc<Link>), BridgeError> {
        let permit = self.reserve_link()?;
        let (id, conn) = self.connect_attached_by_prefix(prefix).await?;
        let link = self.adopt(id, conn, permit).await?;
        Ok((id, link))
    }

    async fn connect_attached_by_prefix(
        &self,
        prefix: &str,
    ) -> Result<(u128, CarrierConnection), BridgeError> {
        let mut conn = dial(&self.target, Offer::ops()).await?;
        // Deliberate: a verb reading a just-exited session's final
        // screen is what the post-exit grace is for.
        let info = conn
            .attach_by_prefix(prefix.to_owned(), AttachIntent::Deliberate)
            .await
            .map_err(|err| {
                BridgeError::new(
                    ErrorKind::from_connect_error(&err),
                    format!("attach `{prefix}`: {err}"),
                )
            })?;
        Ok((info.id, conn))
    }

    /// Install a freshly attached connection as the session's link, or
    /// give it up for the live one another operation installed while
    /// this dial was in flight.
    pub(super) async fn adopt(
        &self,
        id: u128,
        mut conn: CarrierConnection,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<Arc<Link>, BridgeError> {
        // Checked before the drain, which is the expensive half: a
        // redundant dial should cost a round trip, not a rehydrate.
        if let Some(link) = self.take_live(id).await {
            discard(conn, permit).await;
            return Ok(link);
        }
        if let Err(err) = drain_rehydrate(&mut conn).await {
            discard(conn, permit).await;
            return Err(err);
        }
        Ok(self.install_or_borrow(id, conn, permit).await)
    }

    /// Install a connection as the session's link, or borrow the link
    /// the slot already has and give this connection up. One lock spans
    /// the check and the install: a loser that replaced the winner's
    /// link would leave that carrier open with no permit held for it.
    pub(super) async fn install_or_borrow(
        &self,
        id: u128,
        conn: CarrierConnection,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Arc<Link> {
        let (link, surplus) = {
            let mut slots = self.sessions.lock().await;
            match slots.entry(id) {
                std::collections::hash_map::Entry::Occupied(entry) => {
                    let slot = entry.into_mut();
                    slot.users += 1;
                    if let Some(link) = slot.live() {
                        (link, Some((conn, permit)))
                    } else {
                        let link = Link::start(conn, "session");
                        slot.link = Some(Arc::clone(&link));
                        // The slot's own admission already covers
                        // whatever link it holds.
                        drop(permit);
                        (link, None)
                    }
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let slot = entry.insert(SessionSlot::vacant(permit));
                    slot.users += 1;
                    let link = Link::start(conn, "session");
                    slot.link = Some(Arc::clone(&link));
                    (link, None)
                }
            }
        };
        if let Some((conn, permit)) = surplus {
            discard(conn, permit).await;
        }
        link
    }

    /// Count a user onto the session's live link, if it has one.
    async fn take_live(&self, id: u128) -> Option<Arc<Link>> {
        let mut slots = self.sessions.lock().await;
        let slot = slots.get_mut(&id)?;
        let link = slot.live()?;
        slot.users += 1;
        Some(link)
    }

    /// Counted per slot, not per link: an operation whose link died may
    /// release long after `acquire` replaced it, and counting per link
    /// would let that stale release close the live successor.
    pub(super) async fn release(&self, id: u128) {
        let mut slots = self.sessions.lock().await;
        let Some(slot) = slots.get_mut(&id) else {
            return;
        };
        slot.users -= 1;
        if slot.users > 0 {
            return;
        }
        let closing = slots.remove(&id);
        drop(slots);
        if let Some(slot) = closing {
            let _permit = slot.link_permit;
            if let Some(link) = slot.link {
                // Explicit `Detach` rather than a bare close: the pool
                // re-pools on drop too, but only after the post-exit grace
                // tick, and a following `capture` would find the session
                // still attached.
                link.detach().await;
                link.shutdown().await;
            }
        }
    }

    /// Borrow the session's link for one operation. The attach-scoped
    /// verbs name their session by prefix, and the resolved id rides
    /// back out so a reply can report the session it reached.
    pub(super) async fn with_session_by_prefix<T, F>(
        &self,
        op: &Op,
        prefix: &str,
        body: F,
    ) -> Result<(u128, T), BridgeError>
    where
        F: AsyncFnOnce(Arc<Link>) -> Result<T, BridgeError>,
    {
        let (id, link) = self.acquire_by_prefix(prefix).await?;
        op.track_link(&link);
        let outcome = body(link).await;
        self.release(id).await;
        outcome.map(|value| (id, value))
    }

    pub(super) async fn shutdown(
        &self,
        err: &BridgeError,
        code: i32,
        output_available: bool,
    ) -> i32 {
        let outstanding: Vec<Arc<ActiveOp>> =
            self.active.lock_or_poisoned().values().cloned().collect();

        if output_available {
            for entry in &outstanding {
                entry.stream.lock_or_poisoned().cancel();
            }
            settle(&outstanding, err, SHUTDOWN_GRACE).await;
        } else {
            abort_outstanding(&outstanding).await;
        }
        self.close_links(&outstanding).await;
        code
    }

    async fn close_links(&self, outstanding: &[Arc<ActiveOp>]) {
        self.anchor.shutdown().await;
        let session_links: Vec<Arc<Link>> = self
            .sessions
            .lock()
            .await
            .drain()
            .filter_map(|(_, slot)| slot.link)
            .collect();
        for link in session_links {
            link.shutdown().await;
        }
        let observer_links: Vec<Arc<Link>> = outstanding
            .iter()
            .filter_map(|entry| {
                entry
                    .stream
                    .lock_or_poisoned()
                    .link
                    .as_ref()
                    .map(Arc::clone)
            })
            .collect();
        for link in observer_links {
            link.shutdown().await;
        }
    }
}

/// Wait out the grace for the daemon's own terminals, then answer
/// whatever is still open. The wait borrows each handle: a `JoinHandle`
/// dropped by an elapsed timeout *detaches* its task instead of
/// stopping it, and a stalled `capture` would keep running against the
/// daemon with its terminal already written.
pub(super) async fn settle(
    outstanding: &[Arc<ActiveOp>],
    err: &BridgeError,
    grace: std::time::Duration,
) {
    let deadline = tokio::time::Instant::now() + grace;
    let mut tasks: Vec<Option<tokio::task::JoinHandle<()>>> = outstanding
        .iter()
        .map(|entry| entry.task.lock_or_poisoned().take())
        .collect();
    for slot in &mut tasks {
        let Some(task) = slot.as_mut() else {
            continue;
        };
        // One shared deadline, so the whole shutdown is bounded by the
        // grace and not by the number of operations.
        if tokio::time::timeout_at(deadline, task).await.is_ok() {
            *slot = None;
        }
    }
    for task in tasks.iter().flatten() {
        task.abort();
    }
    for task in tasks.into_iter().flatten() {
        let _joined = task.await;
    }
    for entry in outstanding {
        let _answered = entry.terminate(entry.shutdown_terminal(err), || {});
    }
}

async fn abort_outstanding(outstanding: &[Arc<ActiveOp>]) {
    let tasks: Vec<tokio::task::JoinHandle<()>> = outstanding
        .iter()
        .filter_map(|entry| entry.task.lock_or_poisoned().take())
        .collect();
    for task in &tasks {
        task.abort();
    }
    for task in tasks {
        let _joined = task.await;
    }
}

/// One in-flight operation's view of stdout.
pub(super) struct Op {
    out: Out,
    /// Held so the terminal can free this operation's id where it is
    /// written.
    core: Arc<Core>,
    key: String,
    entry: Arc<ActiveOp>,
}

impl Op {
    fn streaming(&self) -> bool {
        self.entry.streaming
    }

    fn id(&self) -> &Value {
        &self.entry.id
    }

    pub(super) fn track_link(&self, link: &Arc<Link>) {
        self.entry.stream.lock_or_poisoned().link = Some(Arc::clone(link));
    }

    /// A cancel that arrived while the operation was still dialing
    /// fires here.
    pub(super) fn bind_stream(&self, link: &Arc<Link>, id: StreamId) {
        let mut slot = self.entry.stream.lock_or_poisoned();
        slot.link = Some(Arc::clone(link));
        slot.stream_id = Some(id);
        if slot.canceled {
            link.cancel(id);
        }
    }

    pub(super) async fn item(&self, body: Value) -> Result<(), BridgeError> {
        self.entry
            .emit_open(&self.out, envelope(self.id(), None, Body::Item(body)))
            .await
            .map_err(Into::into)
    }

    pub(super) async fn lag(&self, dropped: u64) -> Result<(), BridgeError> {
        self.entry
            .emit_open(
                &self.out,
                envelope(self.id(), Some("lag"), Body::Dropped(dropped)),
            )
            .await
            .map_err(Into::into)
    }

    /// Stdout, like the errors: on the bridge a reply that cannot be
    /// correlated cannot be used.
    pub(super) fn result(&self, body: Value) {
        self.finish(envelope(self.id(), None, Body::Result(body)));
    }

    fn error(&self, err: &BridgeError) {
        self.finish(error_object(self.id(), err));
    }

    /// Spelled exactly as the one-shot verbs spell theirs, with only
    /// the correlation `id` added: one contract in two framings.
    pub(super) fn end(&self, count: u64, exit_code: Option<u32>) {
        self.finish(envelope(
            self.id(),
            Some("end"),
            Body::End { count, exit_code },
        ));
    }

    fn error_terminal(&self, err: &BridgeError) {
        self.finish(error_terminal_object(self.id(), err));
    }

    /// Publication, not task scheduling, owns the id lifetime: the
    /// callback runs only after stdout accepts the JSONL delimiter.
    fn finish(&self, object: String) {
        let core = Arc::clone(&self.core);
        let key = self.key.clone();
        let entry = Arc::clone(&self.entry);
        let _answered = self.entry.terminate(object, move || {
            core.retire(&key, &entry);
        });
    }
}

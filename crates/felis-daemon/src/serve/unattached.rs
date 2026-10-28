//! An `Ops` connection that addresses sessions by id without attaching
//! (`docs/reference/ipc.md` "Connection modes"). Session tasks' stream
//! items and this connection's own replies share one outbox and its
//! bytes-in-flight gauge.

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use felis_protocol::{
    MessageKind,
    codec::{SendCount, send_steps_in},
    limits::{MAX_SEND_BODY_BYTES, MAX_SEND_STEPS},
    messages::{
        ConnToClientMsg, ConnToDaemonMsg, Correlation, Directed as _, InputMsg, OpsToClientMsg,
        OpsToDaemonMsg, RegionToDaemonMsg, RequestId, ResolvedId, SearchToDaemonMsg, SendOutcome,
        SendRefusal, SendStep, StopOutcome, StreamErrorReason, StreamId, Validate as _,
    },
};
use felis_transport::{
    DaemonDriver, Delivery, DriverError, FrameReader, FrameWriter, Incoming, Payload, Undecoded,
};
use tokio::sync::{Notify, mpsc, oneshot};
use tracing::{debug, warn};

use super::session_task::{
    OutEvent, Owner, Prepared, RequesterHandle, RequesterId, SUBSCRIBER_BUFFER_CAP, SessionCmd,
};
use super::{ConnError, CreateCtx, input_reservation_bytes, ops_reply, pump_outbound, request_of};
use crate::SessionId;
use crate::pool::SessionHandle;

/// Serve the connection from `first`, the frame that named a session,
/// until the peer leaves.
pub(super) async fn run<R, W>(
    reader: &mut FrameReader<R>,
    writer: &mut FrameWriter<W>,
    driver: &mut DaemonDriver,
    first: Payload,
    ctx: CreateCtx<'_>,
) -> Result<(), ConnError>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let (tx, mut out_rx) = mpsc::unbounded_channel();
    let mut conn = Unattached {
        ctx,
        id: RequesterId::next(),
        tx,
        buffered: Arc::new(AtomicUsize::new(0)),
        evict: Arc::new(Notify::new()),
        sessions: HashMap::new(),
        streams: HashMap::new(),
        stopping: false,
    };
    let buffered = Arc::clone(&conn.buffered);
    let evict = Arc::clone(&conn.evict);
    let (retired_tx, mut retired_rx) = mpsc::unbounded_channel();
    let mut pump = std::pin::pin!(pump_outbound(writer, &buffered, &mut out_rx, &retired_tx));
    let mut pumped = None;
    let result = tokio::select! {
        biased;
        () = evict.notified() => {
            warn!(conn = ?conn.id, "unattached outbox over cap; ending the connection");
            Ok(())
        }
        res = conn.read_loop(reader, driver, &mut retired_rx, first) => res,
        res = &mut pump => {
            pumped = Some(());
            res
        }
    };
    for handle in conn.sessions.values() {
        // Best-effort: a session that already ended dropped every stream.
        drop(
            handle
                .cmd
                .send(SessionCmd::DropRequester { id: conn.id })
                .await,
        );
    }
    if conn.stopping {
        let shutdown = Arc::clone(&conn.ctx.caps.shutdown);
        // Every sender of the outbox is gone once the sessions dropped
        // this requester, so the pump ends after writing the reply.
        drop(conn);
        if pumped.is_none() {
            drop(pump.await);
        }
        shutdown.fire();
    }
    result
}

/// Who an `Ops::Send` answers: a request, or a `SendAndWait` stream.
#[derive(Clone, Copy)]
enum Answer {
    Request(RequestId),
    Stream(StreamId),
}

struct Unattached<'a> {
    ctx: CreateCtx<'a>,
    id: RequesterId,
    tx: mpsc::UnboundedSender<OutEvent>,
    buffered: Arc<AtomicUsize>,
    evict: Arc<Notify>,
    /// Sessions this connection registered with, each told of it once.
    sessions: HashMap<SessionId, SessionHandle>,
    /// Where a `Conn::Cancel` goes.
    streams: HashMap<StreamId, mpsc::Sender<SessionCmd>>,
    /// Set by an `Ops::Stop` that answered `Stopping`: the shutdown
    /// fires once its reply is written.
    stopping: bool,
}

impl Unattached<'_> {
    async fn read_loop<R: tokio::io::AsyncRead + Unpin>(
        &mut self,
        reader: &mut FrameReader<R>,
        driver: &mut DaemonDriver,
        retired: &mut mpsc::UnboundedReceiver<StreamId>,
        first: Payload,
    ) -> Result<(), ConnError> {
        if self.payload(reader, driver, first).await?.is_break() {
            return Ok(());
        }
        while let Some(frame) = reader.next_frame().await? {
            self.ctx.caps.upgrade.gate.wait_open().await;
            while let Ok(stream) = retired.try_recv() {
                let _freed = driver.retire_stream(stream);
                self.streams.remove(&stream);
            }
            let payload = match driver.classify(&frame)? {
                Incoming::CancelIgnored { stream_id } => {
                    debug!(stream = %stream_id, "cancel for an already-terminated stream");
                    continue;
                }
                Incoming::Control(ConnToDaemonMsg::Cancel { stream_id }) => {
                    if let Some(cmd) = self.streams.get(&stream_id) {
                        drop(
                            cmd.send(SessionCmd::CancelStream {
                                owner: Owner::Conn(self.id),
                                stream: stream_id,
                            })
                            .await,
                        );
                    }
                    continue;
                }
                Incoming::Control(other) => {
                    return Err(ConnError::Driver(DaemonDriver::unexpected(
                        "a frame an unattached connection may send",
                        other.variant(),
                    )));
                }
                Incoming::Payload(payload) => payload,
            };
            if self.payload(reader, driver, payload).await?.is_break() {
                return Ok(());
            }
        }
        Ok(())
    }

    /// As [`super::AttachedSub::queue`]; `false` ends the connection.
    fn queue(&self, ev: OutEvent) -> bool {
        let len = ev.approx_wire_len();
        if self.buffered.fetch_add(len, Ordering::Relaxed) + len > SUBSCRIBER_BUFFER_CAP {
            warn!(conn = ?self.id, "reply would overflow the outbox; ending the connection");
            return false;
        }
        self.tx.send(ev).is_ok()
    }

    fn queue_control(&self, msg: Option<ConnToClientMsg>) -> ControlFlow<()> {
        match msg {
            None => ControlFlow::Continue(()),
            Some(msg) => {
                if self.queue(OutEvent::Control(msg)) {
                    ControlFlow::Continue(())
                } else {
                    ControlFlow::Break(())
                }
            }
        }
    }

    async fn payload<R: tokio::io::AsyncRead + Unpin>(
        &mut self,
        reader: &mut FrameReader<R>,
        driver: &mut DaemonDriver,
        payload: Payload,
    ) -> Result<ControlFlow<()>, ConnError> {
        match payload.kind {
            MessageKind::Search => match driver.decode::<SearchToDaemonMsg>(&payload)? {
                Delivery::Deliver(delivered) => {
                    let stream = delivered.stream();
                    let SearchToDaemonMsg::Query { session, .. } = &delivered.msg;
                    let Some(session) = *session else {
                        return Ok(self.queue_control(driver.fail_stream(
                            stream,
                            StreamErrorReason::InvalidRequest,
                            "a search on an unattached connection names its session",
                        )));
                    };
                    let owner = Owner::Conn(self.id);
                    let cmd = SessionCmd::Search {
                        owner,
                        msg: delivered.msg,
                        stream,
                    };
                    Ok(self.open_stream(driver, session, stream, cmd).await)
                }
                Delivery::DroppedAfterCancel => Ok(ControlFlow::Continue(())),
                Delivery::RefuseStream { reply } => Ok(self.queue_control(Some(reply))),
            },
            MessageKind::Region => match driver.decode::<RegionToDaemonMsg>(&payload)? {
                Delivery::Deliver(delivered) => {
                    let owner = Owner::Conn(self.id);
                    match delivered.msg {
                        RegionToDaemonMsg::Request {
                            source,
                            ansi,
                            session,
                        } => {
                            let request = delivered.request();
                            let Some(session) = session else {
                                return Ok(self.queue_control(Some(DaemonDriver::refuse_request(
                                    request,
                                    StreamErrorReason::InvalidRequest,
                                    "a capture on an unattached connection names its session",
                                ))));
                            };
                            let cmd = SessionCmd::Region {
                                owner,
                                source,
                                ansi,
                                request,
                            };
                            Ok(self.request(session, request, cmd).await)
                        }
                        RegionToDaemonMsg::Rows {
                            source,
                            ansi,
                            max_rows,
                            session,
                        } => {
                            let stream = delivered.stream();
                            let Some(session) = session else {
                                return Ok(self.queue_control(driver.fail_stream(
                                    stream,
                                    StreamErrorReason::InvalidRequest,
                                    "a capture on an unattached connection names its session",
                                )));
                            };
                            let cmd = SessionCmd::RegionRows {
                                owner,
                                source,
                                ansi,
                                max_rows,
                                stream,
                            };
                            Ok(self.open_stream(driver, session, stream, cmd).await)
                        }
                    }
                }
                Delivery::DroppedAfterCancel => Ok(ControlFlow::Continue(())),
                Delivery::RefuseStream { reply } => Ok(self.queue_control(Some(reply))),
            },
            MessageKind::Ops => self.ops(reader, driver, &payload).await,
            kind => Err(ConnError::Driver(DaemonDriver::unexpected(
                "an unattached connection's Ops, Search or Region frame",
                kind.as_str(),
            ))),
        }
    }

    /// The task behind `session`, told of this connection the first time.
    async fn session_cmd(&mut self, session: u128) -> Option<mpsc::Sender<SessionCmd>> {
        let id = SessionId(session);
        if let Some(handle) = self.sessions.get(&id) {
            return Some(handle.cmd.clone());
        }
        let handle = self.ctx.pool.lock().await.handle_cloned(id)?;
        self.register(id, handle).await
    }

    async fn register(
        &mut self,
        id: SessionId,
        handle: SessionHandle,
    ) -> Option<mpsc::Sender<SessionCmd>> {
        if let Some(known) = self.sessions.get(&id) {
            return Some(known.cmd.clone());
        }
        let requester = RequesterHandle {
            id: self.id,
            tx: self.tx.clone(),
            buffered: Arc::clone(&self.buffered),
            evict: Arc::clone(&self.evict),
        };
        handle
            .cmd
            .send(SessionCmd::AddRequester(requester))
            .await
            .ok()?;
        let cmd = handle.cmd.clone();
        self.sessions.insert(id, handle);
        Some(cmd)
    }

    async fn open_stream(
        &mut self,
        driver: &mut DaemonDriver,
        session: u128,
        stream: StreamId,
        cmd: SessionCmd,
    ) -> ControlFlow<()> {
        let Some(tx) = self.session_cmd(session).await else {
            return self.queue_control(driver.fail_stream(
                stream,
                StreamErrorReason::Unavailable,
                format!("no session with id {session:#x}"),
            ));
        };
        let dispatching = self.ctx.caps.upgrade.gate.dispatch().await;
        let sent = tx.send(cmd).await;
        drop(dispatching);
        if sent.is_err() {
            return self.queue_control(driver.fail_stream(
                stream,
                StreamErrorReason::Unavailable,
                "the session ended",
            ));
        }
        self.streams.insert(stream, tx);
        ControlFlow::Continue(())
    }

    async fn request(
        &mut self,
        session: u128,
        request: RequestId,
        cmd: SessionCmd,
    ) -> ControlFlow<()> {
        let refusal = match self.session_cmd(session).await {
            None => format!("no session with id {session:#x}"),
            Some(tx) => {
                let dispatching = self.ctx.caps.upgrade.gate.dispatch().await;
                let sent = tx.send(cmd).await;
                drop(dispatching);
                if sent.is_ok() {
                    return ControlFlow::Continue(());
                }
                "the session ended".to_owned()
            }
        };
        self.queue_control(Some(DaemonDriver::refuse_request(
            request,
            StreamErrorReason::Unavailable,
            refusal,
        )))
    }

    async fn ops<R: tokio::io::AsyncRead + Unpin>(
        &mut self,
        reader: &mut FrameReader<R>,
        driver: &mut DaemonDriver,
        payload: &Payload,
    ) -> Result<ControlFlow<()>, ConnError> {
        // Counted before the decode: a body past either bound is
        // refused without materializing its steps.
        if let Ok(Some(count)) = send_steps_in(&payload.body)
            && (count.steps > MAX_SEND_STEPS || payload.body.len() > MAX_SEND_BODY_BYTES)
        {
            return self.refuse_undecoded(driver, payload, count);
        }
        let decoded = match driver.decode::<OpsToDaemonMsg>(payload) {
            Ok(decoded) => decoded,
            Err(DriverError::ArmDenied { mode, .. }) => {
                let request = request_of(payload)?;
                return Ok(self.queue_control(Some(DaemonDriver::refuse_request(
                    request,
                    StreamErrorReason::InvalidRequest,
                    format!("a {mode:?} connection may not operate on other sessions"),
                ))));
            }
            Err(err) => return Err(err.into()),
        };
        let delivered = match decoded {
            Delivery::Deliver(delivered) => delivered,
            Delivery::DroppedAfterCancel => return Ok(ControlFlow::Continue(())),
            Delivery::RefuseStream { reply } => return Ok(self.queue_control(Some(reply))),
        };
        let correlation = delivered.correlation();
        match delivered.msg {
            OpsToDaemonMsg::Send { id_prefix, steps } => {
                let answer = Answer::Request(request_id(correlation));
                Ok(self.send(reader, driver, answer, &id_prefix, steps).await)
            }
            OpsToDaemonMsg::SendAndWait { id_prefix, steps } => {
                let Some(Correlation::Stream(stream)) = correlation else {
                    unreachable!("the driver admits SendAndWait only with a stream id")
                };
                let answer = Answer::Stream(stream);
                Ok(self.send(reader, driver, answer, &id_prefix, steps).await)
            }
            msg => {
                let correlation = Correlation::request(request_id(correlation));
                let dispatching = self.ctx.caps.upgrade.gate.dispatch().await;
                let reply = ops_reply(self.ctx, msg).await;
                drop(dispatching);
                let stopping = matches!(
                    reply,
                    OpsToClientMsg::StopReply {
                        outcome: StopOutcome::Stopping
                    }
                );
                let queued = self.queue(OutEvent::Ops {
                    msg: Box::new(reply),
                    correlation,
                });
                if stopping {
                    self.stopping = true;
                    return Ok(ControlFlow::Break(()));
                }
                Ok(if queued {
                    ControlFlow::Continue(())
                } else {
                    ControlFlow::Break(())
                })
            }
        }
    }

    fn refuse_undecoded(
        &self,
        driver: &mut DaemonDriver,
        payload: &Payload,
        count: SendCount,
    ) -> Result<ControlFlow<()>, ConnError> {
        let arm = if count.wait {
            OpsToDaemonMsg::SendAndWait {
                id_prefix: String::new(),
                steps: Vec::new(),
            }
        } else {
            OpsToDaemonMsg::Send {
                id_prefix: String::new(),
                steps: Vec::new(),
            }
        };
        let correlation = match driver.admit_undecoded(payload, &arm.meta())? {
            Undecoded::Admitted(correlation) => correlation,
            Undecoded::Refused(reply) => return Ok(self.queue_control(Some(reply))),
        };
        let detail = format!(
            "a send carries at most {MAX_SEND_STEPS} steps in a body of at most \
             {MAX_SEND_BODY_BYTES} bytes; this one has {} steps in {} bytes",
            count.steps,
            payload.body.len()
        );
        let refused = SendOutcome::Refused {
            reason: SendRefusal::TooLarge,
            step: None,
            detail,
        };
        let answer = match correlation {
            Correlation::Request(request) => Answer::Request(request),
            Correlation::Stream(stream) => Answer::Stream(stream),
        };
        Ok(self.answer(driver, answer, refused))
    }

    /// Answer a send that wrote nothing, or the `Sent` of one that
    /// finished. A `SendAndWait` that ends here carries the outcome as
    /// its one item before its terminal.
    fn answer(
        &self,
        driver: &mut DaemonDriver,
        answer: Answer,
        outcome: SendOutcome,
    ) -> ControlFlow<()> {
        match answer {
            Answer::Request(request) => {
                if self.queue(OutEvent::Ops {
                    msg: Box::new(OpsToClientMsg::Sent { outcome }),
                    correlation: Correlation::request(request),
                }) {
                    ControlFlow::Continue(())
                } else {
                    ControlFlow::Break(())
                }
            }
            Answer::Stream(stream) => {
                if let SendOutcome::Refused {
                    reason: SendRefusal::SessionEnded,
                    detail,
                    ..
                } = outcome
                {
                    return self.queue_control(driver.fail_stream(
                        stream,
                        StreamErrorReason::Unavailable,
                        detail,
                    ));
                }
                let detail = refusal_detail(&outcome);
                if !self.queue(OutEvent::Ops {
                    msg: Box::new(OpsToClientMsg::WaitSent { outcome }),
                    correlation: Correlation::stream(stream),
                }) {
                    return ControlFlow::Break(());
                }
                self.queue_control(driver.fail_stream(
                    stream,
                    StreamErrorReason::InvalidRequest,
                    detail,
                ))
            }
        }
    }

    /// `Ops::Send` and `Ops::SendAndWait`: the whole request is checked
    /// before the first step is written, then each step is admitted
    /// against the session's input budget in turn, with reads paused as
    /// an attached connection's input pauses them.
    async fn send<R: tokio::io::AsyncRead + Unpin>(
        &mut self,
        reader: &mut FrameReader<R>,
        driver: &mut DaemonDriver,
        answer: Answer,
        id_prefix: &str,
        steps: Vec<SendStep>,
    ) -> ControlFlow<()> {
        let mut inputs = Vec::with_capacity(steps.len());
        let mut mouse = Vec::new();
        for (index, step) in steps.into_iter().enumerate() {
            let index = u32::try_from(index).unwrap_or(u32::MAX);
            let msg = step.into_input();
            if let Err(refusal) = check_step(&msg) {
                let (reason, detail) = refusal;
                let outcome = SendOutcome::Refused {
                    reason,
                    step: Some(index),
                    detail,
                };
                return self.answer(driver, answer, outcome);
            }
            if let InputMsg::Mouse(event) = &msg {
                mouse.push((index, *event));
            }
            inputs.push(msg);
        }

        let (id, handle) = {
            let guard = self.ctx.pool.lock().await;
            let resolved = felis_protocol::session_prefix::resolve_session_prefix(
                id_prefix,
                guard.ids().map(|id| id.0),
            );
            let found = match resolved {
                ResolvedId::Ok { id } => guard.handle_cloned(SessionId(id)).map(|h| (id, h)),
                ResolvedId::NoMatch => None,
                ResolvedId::Ambiguous { matches } => {
                    drop(guard);
                    return self.answer(driver, answer, SendOutcome::Ambiguous { matches });
                }
            };
            drop(guard);
            let Some(found) = found else {
                return self.answer(driver, answer, SendOutcome::NoMatch);
            };
            found
        };
        let ended = |detail: &str| SendOutcome::Refused {
            reason: SendRefusal::SessionEnded,
            step: None,
            detail: detail.to_owned(),
        };
        let budget = Arc::clone(&handle.input_budget);
        let watch = match answer {
            Answer::Stream(stream) => Some((self.id, stream)),
            Answer::Request(_) => None,
        };
        let cmd = if watch.is_some() {
            match self.register(SessionId(id), handle).await {
                Some(cmd) => cmd,
                None => return self.answer(driver, answer, ended("the session ended")),
            }
        } else {
            handle.cmd
        };

        let (reply_tx, reply_rx) = oneshot::channel();
        let prepare = SessionCmd::PrepareSend {
            watch,
            mouse,
            reply: reply_tx,
        };
        let prepared = match cmd.send(prepare).await {
            Ok(()) => reply_rx.await.unwrap_or(Prepared::Ended),
            Err(_) => Prepared::Ended,
        };
        match prepared {
            Prepared::Ready => {}
            Prepared::Ended => {
                return self.answer(driver, answer, ended("the session has ended"));
            }
            Prepared::Unreported { step, detail } => {
                let outcome = SendOutcome::Refused {
                    reason: SendRefusal::Unreported,
                    step: Some(step),
                    detail,
                };
                return self.answer(driver, answer, outcome);
            }
        }
        // From here a `SendAndWait` stream's terminal is the session
        // task's to write: it registered the watch.
        if let Answer::Stream(stream) = answer {
            self.streams.insert(stream, cmd.clone());
        }

        for msg in inputs {
            let reservation = match input_reservation_bytes(&msg) {
                Ok(Some(bytes)) => {
                    let permit = tokio::select! {
                        biased;
                        granted = Arc::clone(&budget).acquire_many_owned(bytes) => {
                            let Ok(permit) = granted else { return ControlFlow::Break(()) };
                            permit
                        }
                        hangup = reader.wait_for_hangup() => {
                            if let Err(err) = hangup {
                                debug!(?err, "unattached connection lost mid-send");
                            }
                            return ControlFlow::Break(());
                        }
                        () = cmd.closed() => {
                            return self.ended_mid_send(driver, answer, ended("the session ended"));
                        }
                    };
                    let reservation: crate::pool::InputReservation = Box::new(permit);
                    Some(reservation)
                }
                // `check_step` refused every step this could fail on.
                Ok(None) | Err(_) => None,
            };
            let dispatching = self.ctx.caps.upgrade.gate.dispatch().await;
            let sent = cmd.send(SessionCmd::RemoteInput { msg, reservation }).await;
            drop(dispatching);
            if sent.is_err() {
                return self.ended_mid_send(driver, answer, ended("the session ended"));
            }
        }

        let (fence_tx, fence_rx) = oneshot::channel();
        let fenced = cmd
            .send(SessionCmd::SendFence {
                watch,
                id,
                reply: fence_tx,
            })
            .await
            .is_ok()
            && fence_rx.await.unwrap_or(false);
        if !fenced {
            return self.ended_mid_send(
                driver,
                answer,
                ended("the session ended before it took the input"),
            );
        }
        match answer {
            Answer::Request(_) => self.answer(driver, answer, SendOutcome::Sent { id }),
            Answer::Stream(_) => ControlFlow::Continue(()),
        }
    }

    /// A `SendAndWait` stream is already ended by the session task that
    /// held its watch.
    fn ended_mid_send(
        &self,
        driver: &mut DaemonDriver,
        answer: Answer,
        outcome: SendOutcome,
    ) -> ControlFlow<()> {
        match answer {
            Answer::Request(_) => self.answer(driver, answer, outcome),
            Answer::Stream(_) => ControlFlow::Continue(()),
        }
    }
}

/// One step's own bounds, as a window's input frame is held to them.
fn check_step(msg: &InputMsg) -> Result<(), (SendRefusal, String)> {
    msg.validate()
        .map_err(|err| (SendRefusal::InvalidStep, err.to_string()))?;
    input_reservation_bytes(msg)
        .map(drop)
        .map_err(|err| (SendRefusal::TooLarge, err.to_string()))
}

fn refusal_detail(outcome: &SendOutcome) -> String {
    match outcome {
        SendOutcome::Refused { detail, .. } => detail.clone(),
        SendOutcome::NoMatch => "no session id matches".to_owned(),
        SendOutcome::Ambiguous { matches } => format!("the prefix matches {matches} sessions"),
        SendOutcome::Sent { .. } => String::new(),
    }
}

fn request_id(correlation: Option<Correlation>) -> RequestId {
    match correlation {
        Some(Correlation::Request(id)) => id,
        found => {
            unreachable!("the driver admits an Ops request only with a request id, not {found:?}")
        }
    }
}

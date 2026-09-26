use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use felis_client_core::{
    CarrierConnection, CarrierReader, CarrierWriter, Connection, Offer, OpenFrom, OpenStreamError,
    Reconnector, begin_stream, open_stream,
};
use felis_protocol::{
    MessageKind, codec,
    messages::{
        ConnToClientMsg, ConnToDaemonMsg, Correlation, InputMsg, NotifyToClientMsg,
        NotifyToDaemonMsg, OpsToClientMsg, RegionToClientMsg, SearchToClientMsg,
        SessionToClientMsg, SessionToDaemonMsg, StreamErrorReason, StreamId, Subject,
    },
};
use felis_transport::{
    ClientDriver, Delivery, DriverError, FrameReader, FrameWriter, Incoming, OwnedFrame, Payload,
};
use tokio::sync::{mpsc, oneshot};

use super::{
    LockOrPoisoned as _, MAX_IN_FLIGHT, REMOTE_SPAWN, STREAM_BUFFER_CAP,
    admission::{preflight_correlated, preflight_uncorrelated},
    envelope::BridgeError,
};
use crate::cli_output::ErrorKind;
use crate::conn::RehydrateError;

/// One daemon connection, multiplexed by the wire's own correlation
/// ids: a single reader task matches every inbound frame back to the
/// operation waiting for it.
pub(super) struct Link {
    label: &'static str,
    pub(super) writer: tokio::sync::Mutex<Option<FrameWriter<CarrierWriter>>>,
    pub(super) driver: std::sync::Mutex<ClientDriver>,
    pub(super) registry: std::sync::Mutex<Registry>,
    lost: tokio::sync::Notify,
    pump: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub(super) cancel_writes: std::sync::Mutex<CancelWrites>,
}

#[derive(Default)]
pub(super) struct CancelWrites {
    stopping: bool,
    pub(super) tasks: Vec<tokio::task::JoinHandle<()>>,
}

#[derive(Default)]
pub(super) struct Registry {
    pub(super) requests: HashMap<u64, oneshot::Sender<Result<DaemonMsg, LinkError>>>,
    pub(super) streams: HashMap<u64, mpsc::Sender<StreamEvent>>,
    /// Every later registration fails immediately rather than waiting
    /// for a reply that cannot come.
    lost: Option<LinkError>,
}

#[derive(Debug, Clone)]
pub(super) enum LinkError {
    InvalidRequest(String),
    Lost(String),
    Protocol(String),
    Refused {
        reason: StreamErrorReason,
        detail: String,
    },
}

/// The reader knows the family from the frame's kind word, so the
/// decode happens once, where the driver's accounting also has to.
#[derive(Debug)]
pub(super) enum DaemonMsg {
    Ops(OpsToClientMsg),
    Session(SessionToClientMsg),
    Region(RegionToClientMsg),
    Search(SearchToClientMsg),
    Notify(NotifyToClientMsg),
}

impl DaemonMsg {
    pub(super) const fn kind(&self) -> &'static str {
        match self {
            Self::Ops(_) => "Ops",
            Self::Session(_) => "Session",
            Self::Region(_) => "Region",
            Self::Search(_) => "Search",
            Self::Notify(_) => "Notify",
        }
    }
}

pub(super) enum StreamEvent {
    /// Boxed: an item is an order of magnitude larger than either
    /// terminal.
    Item(Box<StreamItem>),
    End {
        count: u32,
    },
    Failed(LinkError),
}

/// The typed value is what a verb matches on; the raw body is what
/// [`capture_row_json`](super::ops::capture_row_json) turns into the
/// row shape.
pub(super) struct StreamItem {
    pub(super) msg: DaemonMsg,
    pub(super) payload: Payload,
}

pub(super) struct StreamHandle {
    pub(super) id: StreamId,
    rx: mpsc::Receiver<StreamEvent>,
}

impl StreamHandle {
    /// A closed channel means the link died without getting a terminal
    /// out, which is itself the terminal condition, so this never
    /// returns `None`.
    pub(super) async fn next(&mut self) -> StreamEvent {
        self.rx.recv().await.unwrap_or_else(|| {
            StreamEvent::Failed(LinkError::Lost(
                "the daemon connection ended without a stream terminal".to_owned(),
            ))
        })
    }
}

impl Link {
    pub(super) fn start(conn: CarrierConnection, label: &'static str) -> Arc<Self> {
        let Connection {
            reader,
            writer,
            driver,
            ..
        } = conn;
        let link = Arc::new(Self {
            label,
            writer: tokio::sync::Mutex::new(Some(writer)),
            driver: std::sync::Mutex::new(driver),
            registry: std::sync::Mutex::new(Registry::default()),
            lost: tokio::sync::Notify::new(),
            pump: std::sync::Mutex::new(None),
            cancel_writes: std::sync::Mutex::new(CancelWrites::default()),
        });
        let pump = tokio::spawn(pump(Arc::clone(&link), reader));
        *link.pump.lock_or_poisoned() = Some(pump);
        link
    }

    pub(super) fn is_lost(&self) -> bool {
        self.registry.lock_or_poisoned().lost.is_some()
    }

    pub(super) async fn lost(&self) {
        loop {
            let notified = self.lost.notified();
            if self.is_lost() {
                return;
            }
            notified.await;
            if self.is_lost() {
                return;
            }
        }
    }

    /// Allocate, register, and send under the writer lock.
    ///
    /// Request IDs are strictly positional; locking prevents concurrent tasks
    /// from interleaving allocation and send.
    pub(super) async fn request<M>(&self, msg: &M) -> Result<DaemonMsg, LinkError>
    where
        M: codec::Correlated
            + felis_protocol::MinorGated
            + felis_protocol::messages::Directed
            + Sync,
    {
        preflight_correlated(msg).map_err(|err| LinkError::InvalidRequest(err.message))?;
        let mut writer = self.writer.lock().await;
        let Some(open_writer) = writer.as_mut() else {
            return Err(self.closed_writer());
        };
        // Before the allocation, not after: an id issued for a frame
        // that is never sent is the skip the daemon refuses.
        let lost = self.registry.lock_or_poisoned().lost.clone();
        if let Some(err) = lost {
            return Err(err);
        }
        let id = {
            let mut driver = self.driver.lock_or_poisoned();
            driver
                .issue_request()
                .map_err(|err| LinkError::Protocol(err.to_string()))?
        };
        let (tx, rx) = oneshot::channel();
        {
            let mut registry = self.registry.lock_or_poisoned();
            if let Some(err) = registry.lost.clone() {
                return Err(err);
            }
            // Registered before the write: the reply can land on the
            // reader task the instant the bytes leave, and an
            // unregistered id is corruption to the driver.
            let _replaced = registry.requests.insert(id.get(), tx);
        }
        let written = open_writer
            .send_correlated(msg, Correlation::request(id))
            .await;
        self.finish_write(writer, written).await?;
        rx.await
            .unwrap_or_else(|_| Err(LinkError::Lost("the link ended mid-request".to_owned())))
    }

    pub(super) async fn open_stream<M>(&self, msg: &M) -> Result<StreamHandle, LinkError>
    where
        M: codec::Correlated
            + felis_protocol::MinorGated
            + felis_protocol::messages::Directed
            + Sync,
    {
        self.open_stream_from(OpenFrom::Attached, msg).await
    }

    /// The `Setup` → `Observing` transition rides inside the open, so
    /// it lands before the subscribe is written rather than after its
    /// ack: the reader task classifies on its own, and transitioning
    /// after the send would race the ack and refuse it as out of phase.
    pub(super) async fn subscribe_notifications(
        &self,
        session_prefix: Option<String>,
    ) -> Result<StreamHandle, LinkError> {
        self.open_stream_from(
            OpenFrom::Setup,
            &NotifyToDaemonMsg::Subscribe { session_prefix },
        )
        .await
    }

    /// Allocate, register, then send, all under the writer lock: a
    /// stream's id is *positional* on the wire, so two concurrent opens
    /// on one link must not swap places between allocation and send.
    async fn open_stream_from<M>(&self, from: OpenFrom, msg: &M) -> Result<StreamHandle, LinkError>
    where
        M: codec::Correlated
            + felis_protocol::MinorGated
            + felis_protocol::messages::Directed
            + Sync,
    {
        preflight_correlated(msg).map_err(|err| LinkError::InvalidRequest(err.message))?;
        let mut writer = self.writer.lock().await;
        let Some(open_writer) = writer.as_mut() else {
            return Err(self.closed_writer());
        };
        let lost = self.registry.lock_or_poisoned().lost.clone();
        if let Some(err) = lost {
            return Err(err);
        }
        let (tx, rx) = mpsc::channel(STREAM_BUFFER_CAP);
        let opened = open_stream(open_writer, msg, || {
            let id = {
                let mut driver = self.driver.lock_or_poisoned();
                begin_stream(&mut driver, from)
                    .map_err(|err| LinkError::Protocol(err.to_string()))?
            };
            let mut registry = self.registry.lock_or_poisoned();
            if let Some(err) = registry.lost.clone() {
                return Err(err);
            }
            // Registered before the write: the reader task matches the
            // first item the instant the bytes leave, and an
            // unregistered id is corruption to the driver.
            let _replaced = registry.streams.insert(id.get(), tx);
            Ok(id)
        })
        .await;
        match opened {
            Ok(id) => Ok(StreamHandle { id, rx }),
            Err(OpenStreamError::Invalid(err)) => Err(LinkError::InvalidRequest(err.to_string())),
            Err(OpenStreamError::Begin(err)) => Err(err),
            Err(OpenStreamError::Write(err)) => Err(self.writer_died(writer, err).await),
        }
    }

    /// Best-effort by contract: items already on the wire keep arriving
    /// until the terminal lands, and a cancel that crosses the terminal
    /// is an idempotent no-op on the far side.
    pub(super) fn cancel(self: &Arc<Self>, id: StreamId) {
        self.driver.lock_or_poisoned().cancel_stream(id);
        let mut writes = self.cancel_writes.lock_or_poisoned();
        if writes.stopping {
            return;
        }
        writes.tasks.retain(|task| !task.is_finished());
        if writes.tasks.len() >= MAX_IN_FLIGHT {
            return;
        }
        let link = Arc::clone(self);
        let task = tokio::spawn(async move {
            let _sent = link
                .write_uncorrelated(&ConnToDaemonMsg::Cancel { stream_id: id })
                .await;
        });
        writes.tasks.push(task);
    }

    async fn send<M>(&self, msg: &M) -> Result<(), BridgeError>
    where
        M: codec::WireCodec
            + felis_protocol::MinorGated
            + felis_protocol::messages::Directed
            + Sync,
    {
        self.write_uncorrelated(msg).await.map_err(Into::into)
    }

    /// Write input and wait for the daemon to admit it.
    ///
    /// The daemon processes frames in order; a `Session::InputFence` issued
    /// behind the input confirms the frame cleared admission before the
    /// connection can detach.
    pub(super) async fn send_input(&self, msg: &InputMsg) -> Result<(), BridgeError> {
        self.send(msg).await?;
        match self.request(&SessionToDaemonMsg::InputFence).await? {
            DaemonMsg::Session(SessionToClientMsg::InputAccepted) => Ok(()),
            other => Err(BridgeError::from(LinkError::Protocol(format!(
                "the daemon answered the input fence with a {} frame",
                other.kind()
            )))),
        }
    }

    pub(super) async fn detach(&self) {
        let _sent = self.write_uncorrelated(&SessionToDaemonMsg::Detach).await;
    }

    async fn write_uncorrelated<M>(&self, msg: &M) -> Result<(), LinkError>
    where
        M: codec::WireCodec
            + felis_protocol::MinorGated
            + felis_protocol::messages::Directed
            + Sync,
    {
        preflight_uncorrelated(msg).map_err(|err| LinkError::InvalidRequest(err.message))?;
        let mut writer = self.writer.lock().await;
        let Some(open_writer) = writer.as_mut() else {
            return Err(self.closed_writer());
        };
        let written = open_writer.send(msg).await;
        self.finish_write(writer, written).await
    }

    fn closed_writer(&self) -> LinkError {
        self.registry
            .lock_or_poisoned()
            .lost
            .clone()
            .unwrap_or_else(|| LinkError::Lost("the daemon connection is closed".to_owned()))
    }

    async fn finish_write<T>(
        &self,
        writer: tokio::sync::MutexGuard<'_, Option<FrameWriter<CarrierWriter>>>,
        result: Result<T, felis_transport::TransportError>,
    ) -> Result<T, LinkError> {
        match result {
            Ok(value) => Ok(value),
            Err(err) => Err(self.writer_died(writer, err).await),
        }
    }

    async fn writer_died(
        &self,
        mut writer: tokio::sync::MutexGuard<'_, Option<FrameWriter<CarrierWriter>>>,
        err: felis_transport::TransportError,
    ) -> LinkError {
        let failure = LinkError::Lost(err.to_string());
        writer.take();
        drop(writer);
        self.stop_pump().await;
        self.fail_all(&failure).await;
        failure
    }

    async fn stop_cancel_writes(&self) {
        let tasks = {
            let mut writes = self.cancel_writes.lock_or_poisoned();
            writes.stopping = true;
            std::mem::take(&mut writes.tasks)
        };
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _joined = task.await;
        }
    }

    async fn stop_pump(&self) {
        let pump = self.pump.lock_or_poisoned().take();
        if let Some(pump) = pump {
            pump.abort();
            let _joined = pump.await;
        }
    }

    /// The fan-out runs here too: aborting the pump skips the one at
    /// the end of its loop, and a stream that never resolves is a
    /// stream with no terminal.
    pub(super) async fn shutdown(&self) {
        self.stop_cancel_writes().await;
        self.writer.lock().await.take();
        self.stop_pump().await;
        self.fail_all(&LinkError::Lost(
            "the bridge closed this connection".to_owned(),
        ))
        .await;
    }

    async fn dispatch(&self, frame: &OwnedFrame) -> Result<(), LinkError> {
        let incoming = self
            .driver
            .lock_or_poisoned()
            .classify(frame)
            .map_err(|err| protocol_error(&err))?;
        match incoming {
            Incoming::Control(ConnToClientMsg::End { stream_id, count }) => {
                self.deliver_stream(stream_id.get(), StreamEvent::End { count })
                    .await;
            }
            Incoming::Control(ConnToClientMsg::Error {
                subject: Subject::Stream(stream_id),
                reason,
                detail,
            }) => {
                self.deliver_stream(
                    stream_id.get(),
                    StreamEvent::Failed(LinkError::Refused { reason, detail }),
                )
                .await;
            }
            Incoming::Control(ConnToClientMsg::Error {
                subject: Subject::Request(id),
                reason,
                detail,
            }) => {
                self.deliver_request(id.get(), Err(LinkError::Refused { reason, detail }));
            }
            Incoming::Control(_) | Incoming::CancelIgnored { .. } => {}
            Incoming::Payload(payload) => self.dispatch_payload(payload).await?,
        }
        Ok(())
    }

    async fn dispatch_payload(&self, payload: Payload) -> Result<(), LinkError> {
        // Drain uncorrelated session push traffic to avoid stalling the daemon pump.
        // Dropped only after admitting through the driver arm table (REQ-113a).
        if !payload.kind.is_correlated() {
            return self
                .driver
                .lock_or_poisoned()
                .admit_drained(&payload)
                .map_err(|err| protocol_error(&err));
        }
        // The decode is the driver's whole accounting: it retires the
        // request a reply names and refuses an envelope the arm's class
        // forbids. So it runs before the routing below, and for an item
        // this side already canceled too.
        let msg = self.decode(&payload)?;
        match payload.correlation {
            Some(Correlation::Request(id)) => {
                let delivered = msg.ok_or_else(|| {
                    LinkError::Protocol("the daemon replied with a droppable frame".to_owned())
                });
                self.deliver_request(id.get(), delivered);
            }
            Some(Correlation::Stream(id)) => {
                if let Some(msg) = msg {
                    self.deliver_stream(
                        id.get(),
                        StreamEvent::Item(Box::new(StreamItem { msg, payload })),
                    )
                    .await;
                }
            }
            None => {}
        }
        Ok(())
    }

    /// `None` is the driver dropping an item for a stream this side
    /// canceled.
    fn decode(&self, payload: &Payload) -> Result<Option<DaemonMsg>, LinkError> {
        let mut driver = self.driver.lock_or_poisoned();
        macro_rules! decode_as {
            ($ty:ty, $arm:ident) => {
                match driver
                    .decode::<$ty>(payload)
                    .map_err(|err| protocol_error(&err))?
                {
                    Delivery::Deliver(delivered) => Some(DaemonMsg::$arm(delivered.msg)),
                    Delivery::DroppedAfterCancel | Delivery::RefuseStream { .. } => None,
                }
            };
        }
        Ok(match payload.kind {
            MessageKind::Ops => decode_as!(OpsToClientMsg, Ops),
            MessageKind::Session => decode_as!(SessionToClientMsg, Session),
            MessageKind::Region => decode_as!(RegionToClientMsg, Region),
            MessageKind::Search => decode_as!(SearchToClientMsg, Search),
            MessageKind::Notify => decode_as!(NotifyToClientMsg, Notify),
            MessageKind::Conn
            | MessageKind::Input
            | MessageKind::Grid
            | MessageKind::Image
            | MessageKind::Push => None,
        })
    }

    fn deliver_request(&self, id: u64, outcome: Result<DaemonMsg, LinkError>) {
        let waiting = self.registry.lock_or_poisoned().requests.remove(&id);
        if let Some(waiting) = waiting {
            let _delivered = waiting.send(outcome).is_ok();
        }
    }

    pub(super) async fn deliver_stream(&self, id: u64, event: StreamEvent) {
        let terminal = !matches!(event, StreamEvent::Item(_));
        let sender = {
            let mut registry = self.registry.lock_or_poisoned();
            // The terminal closes the entry as it is delivered, so a stream
            // can never be handed a second one.
            if terminal {
                registry.streams.remove(&id)
            } else {
                registry.streams.get(&id).cloned()
            }
        };
        if let Some(sender) = sender {
            let _delivered = sender.send(event).await.is_ok();
        }
    }

    /// One error reply per pending request, one error terminal per open
    /// stream. The operations turn these into their own correlated
    /// objects, which is why the fan-out lives here rather than in the
    /// shutdown path: the shape a client sees depends on what it asked
    /// for, not on how the link died.
    async fn fail_all(&self, err: &LinkError) {
        let (requests, streams) = {
            let mut registry = self.registry.lock_or_poisoned();
            if registry.lost.is_some() {
                return;
            }
            registry.lost = Some(err.clone());
            (
                std::mem::take(&mut registry.requests),
                std::mem::take(&mut registry.streams),
            )
        };
        self.lost.notify_waiters();
        for (_, waiting) in requests {
            let _delivered = waiting.send(Err(err.clone())).is_ok();
        }
        for (_, sender) in streams {
            let _delivered = sender.send(StreamEvent::Failed(err.clone())).await.is_ok();
        }
    }
}

fn protocol_error(err: &DriverError) -> LinkError {
    LinkError::Protocol(err.to_string())
}

async fn pump(link: Arc<Link>, mut reader: FrameReader<CarrierReader>) {
    let err = loop {
        match reader.next_frame().await {
            Ok(Some(frame)) => {
                if let Err(err) = link.dispatch(&frame).await {
                    break err;
                }
            }
            Ok(None) => break LinkError::Lost("the daemon closed the connection".to_owned()),
            Err(err) => break LinkError::Lost(err.to_string()),
        }
    };
    tracing::warn!(link = link.label, error = ?err, "bridge: daemon link ended");
    link.fail_all(&err).await;
}

pub(super) async fn dial(
    target: &Reconnector,
    offer: Offer,
) -> Result<CarrierConnection, BridgeError> {
    let descriptor = Reconnector {
        carrier: target.carrier.clone(),
        offer,
    };
    crate::conn::dial(&descriptor, REMOTE_SPAWN)
        .await
        .map_err(|err| {
            // A full daemon gets the kind a spawn past the session cap
            // already reports: "wait and retry" is a different remedy from
            // "there is no daemon".
            felis_client_core::refusal_detail(&err).map_or_else(
                || BridgeError::daemon_unreachable(target, &err),
                |(reason, detail)| {
                    BridgeError::new(
                        ErrorKind::from_refusal(reason),
                        format!("the felis daemon refused a connection: {detail}"),
                    )
                },
            )
        })
}

/// Give up a connection that will not become a link. Explicit `Detach`
/// rather than a bare close, for the reason `Core::release` gives.
async fn close_carrier(conn: CarrierConnection) {
    let link = Link::start(conn, "session");
    link.detach().await;
    link.shutdown().await;
}

/// Give up a connection along with the admission it took, releasing
/// that admission only once the carrier is closed: dropping the permit
/// first would let a replacement dial against a subscriber this bridge
/// still holds.
pub(super) async fn discard(conn: CarrierConnection, permit: tokio::sync::OwnedSemaphorePermit) {
    close_carrier(conn).await;
    drop(permit);
}

/// Drained before the connection becomes a `Link` because the burst
/// has no correlation to route by, and the first `send` on this link
/// must not race it.
pub(super) async fn drain_rehydrate(conn: &mut CarrierConnection) -> Result<(), BridgeError> {
    crate::conn::drain_rehydrate(conn)
        .await
        .map_err(|err| match err {
            RehydrateError::Closed => BridgeError::new(
                ErrorKind::DaemonLost,
                "the daemon closed the connection mid-sync",
            ),
            RehydrateError::Read(err) => BridgeError::new(ErrorKind::Protocol, err.to_string()),
            RehydrateError::TimedOut => {
                BridgeError::new(ErrorKind::Timeout, "timed out syncing the session's screen")
            }
            RehydrateError::Corrupt(err) => BridgeError::new(ErrorKind::Protocol, err.to_string()),
        })
}

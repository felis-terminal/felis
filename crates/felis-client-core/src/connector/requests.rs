use felis_protocol::{
    codec::{self, WireCodec},
    messages::{
        AttachFailure, AttachRefusal, AttachTarget, Directed, InfoOutcome, NotifyToDaemonMsg,
        OpsToClientMsg, OpsToDaemonMsg, ResolvedId, ResourceReport, RetargetTarget, SessionInfo,
        SessionToClientMsg, SessionToDaemonMsg, SpawnArgs, SpawnOutcome, StopMode, StopOutcome,
        StreamId, SwitchDenied, SwitchScope, SwitchTarget, UpgradeOutcome,
    },
    minor::MinorGated,
};
use tokio::io::{AsyncRead, AsyncWrite};

use super::{ConnectError, Connection, check_arm_minor};
use crate::stream::OpenFrom;

/// What was asked, as far as which refusals can answer it.
///
/// A create's context does not name its arguments: the daemon's spawn
/// half can refuse any of them, and its attach half can only lose the
/// race against a child that ended first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRefusalContext {
    Attach { by_prefix: bool, live_only: bool },
    Create,
}

/// Whether the daemon could have reached `reason` from `ctx`: exactly
/// the set `felis-daemon`'s `serve.rs` produces per request form, at
/// value level because an arm is too coarse (a create's attach half
/// can only lose the reap race, and `SessionExited` needs `live_only`).
/// Anything outside it is a protocol failure, never a merits decision.
#[must_use]
pub const fn admits(ctx: SessionRefusalContext, reason: AttachRefusal) -> bool {
    match (ctx, reason) {
        // A create's spawn half may refuse for any of its own reasons;
        // its attach half can only lose the reap race, which is also
        // the one attach-half reason neither target form rules out.
        (SessionRefusalContext::Create, AttachRefusal::Create(_))
        | (
            SessionRefusalContext::Create | SessionRefusalContext::Attach { .. },
            AttachRefusal::Attach(AttachFailure::SessionEnding),
        ) => true,
        (
            SessionRefusalContext::Attach { by_prefix, .. },
            AttachRefusal::Attach(AttachFailure::UnknownSession),
        ) => !by_prefix,
        (
            SessionRefusalContext::Attach { by_prefix, .. },
            AttachRefusal::Attach(AttachFailure::NoMatch | AttachFailure::Ambiguous),
        ) => by_prefix,
        (
            SessionRefusalContext::Attach { live_only, .. },
            AttachRefusal::Attach(AttachFailure::SessionExited),
        ) => live_only,
        (SessionRefusalContext::Attach { .. }, AttachRefusal::Create(_))
        | (SessionRefusalContext::Create, AttachRefusal::Attach(_)) => false,
    }
}

/// The typed refusal a request earned, or the protocol failure a
/// refusal it could not have earned is.
const fn refusal_error(
    ctx: SessionRefusalContext,
    reason: AttachRefusal,
    detail: String,
) -> ConnectError {
    if !admits(ctx, reason) {
        return ConnectError::InadmissibleRefusal {
            ctx,
            reason,
            detail,
        };
    }
    match reason {
        AttachRefusal::Attach(reason) => ConnectError::AttachFailed { reason, detail },
        AttachRefusal::Create(reason) => ConnectError::CreateFailed { reason, detail },
    }
}

pub(super) trait ControlRequest: Send {
    /// `Sync` keeps the round-trip future `Send` (`&Msg` crosses the
    /// write await; `clippy::future_not_send`).
    type Msg: WireCodec + Directed + MinorGated + Clone + Send + Sync;
    /// The daemon's half of the same family, which the answer arrives in.
    type ReplyMsg: WireCodec + Directed + Send;
    type Reply: Send;
    /// Takes the payload out of the request, leaving the request
    /// itself for [`Self::interpret`] to read the refusal context off.
    fn take_msg(&mut self) -> Self::Msg;
    /// `&self` rather than an associated function: which refusals the
    /// daemon may answer with depends on what was asked, and only the
    /// request knows that (`SessionRefusalContext`).
    fn interpret(&self, reply: Self::ReplyMsg) -> Result<Self::Reply, ConnectError>;
}

/// A request whose reply is matched by `request_id` rather than by
/// arriving next. `Session::Attach` / `Create` do not implement it: they
/// happen once per connection, in the attach phase the driver sequences.
pub(super) trait CorrelatedRequest: ControlRequest
where
    Self::Msg: codec::Correlated,
{
}

/// Intent for attaching to a session.
///
/// [`Self::Automatic`] attaches to live sessions only; [`Self::Deliberate`]
/// may attach to an exited session during post-exit grace to read its final
/// screen (`architecture/session-lifecycle.md` "Post-exit reaping").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachIntent {
    Deliberate,
    Automatic,
}

impl AttachIntent {
    #[must_use]
    pub const fn live_only(self) -> bool {
        matches!(self, Self::Automatic)
    }
}

struct AttachSessionReq {
    target: AttachTarget,
    live_only: bool,
}

impl ControlRequest for AttachSessionReq {
    type Msg = SessionToDaemonMsg;
    type ReplyMsg = SessionToClientMsg;
    type Reply = SessionInfo;
    fn take_msg(&mut self) -> SessionToDaemonMsg {
        SessionToDaemonMsg::Attach {
            target: self.target.clone(),
            live_only: self.live_only,
        }
    }

    fn interpret(&self, reply: SessionToClientMsg) -> Result<Self::Reply, ConnectError> {
        match reply {
            SessionToClientMsg::Attached { info } => Ok(info),
            SessionToClientMsg::AttachFailed { reason, detail } => Err(refusal_error(
                SessionRefusalContext::Attach {
                    by_prefix: matches!(self.target, AttachTarget::Prefix(_)),
                    live_only: self.live_only,
                },
                reason,
                detail,
            )),
            _ => Err(ConnectError::NotSessionAttached),
        }
    }
}

struct CreateSessionReq {
    args: SpawnArgs,
}

impl ControlRequest for CreateSessionReq {
    type Msg = SessionToDaemonMsg;
    type ReplyMsg = SessionToClientMsg;
    type Reply = SessionInfo;
    fn take_msg(&mut self) -> SessionToDaemonMsg {
        SessionToDaemonMsg::Create {
            args: std::mem::take(&mut self.args),
        }
    }

    fn interpret(&self, reply: SessionToClientMsg) -> Result<Self::Reply, ConnectError> {
        match reply {
            SessionToClientMsg::Created { info } => Ok(info),
            SessionToClientMsg::AttachFailed { reason, detail } => {
                Err(refusal_error(SessionRefusalContext::Create, reason, detail))
            }
            _ => Err(ConnectError::NotSessionAttached),
        }
    }
}

struct SpawnSessionReq {
    args: SpawnArgs,
}

impl CorrelatedRequest for SpawnSessionReq {}

impl ControlRequest for SpawnSessionReq {
    type Msg = OpsToDaemonMsg;
    type ReplyMsg = OpsToClientMsg;
    type Reply = SessionInfo;
    fn take_msg(&mut self) -> OpsToDaemonMsg {
        OpsToDaemonMsg::Spawn {
            args: std::mem::take(&mut self.args),
        }
    }

    fn interpret(&self, reply: OpsToClientMsg) -> Result<Self::Reply, ConnectError> {
        match reply {
            OpsToClientMsg::Spawned {
                outcome: SpawnOutcome::Ok { info },
            } => Ok(*info),
            // No context check: `SpawnRefused.reason` is typed
            // `CreateFailure`, so the wire admits nothing else.
            OpsToClientMsg::Spawned {
                outcome: SpawnOutcome::Refused { reason, detail },
            } => Err(ConnectError::CreateFailed { reason, detail }),
            _ => Err(ConnectError::NotSessionAttached),
        }
    }
}

struct InfoReq {
    id_prefix: String,
}

impl CorrelatedRequest for InfoReq {}

impl ControlRequest for InfoReq {
    type Msg = OpsToDaemonMsg;
    type ReplyMsg = OpsToClientMsg;
    type Reply = InfoOutcome;
    fn take_msg(&mut self) -> OpsToDaemonMsg {
        OpsToDaemonMsg::Info {
            id_prefix: std::mem::take(&mut self.id_prefix),
        }
    }

    fn interpret(&self, reply: OpsToClientMsg) -> Result<Self::Reply, ConnectError> {
        match reply {
            OpsToClientMsg::InfoReply { outcome } => Ok(outcome),
            _ => Err(ConnectError::NotSessionAttached),
        }
    }
}

/// The input barrier: a request whose reply says the session processed
/// every `Input` frame this connection sent ahead of it.
struct FenceReq;

impl CorrelatedRequest for FenceReq {}

impl ControlRequest for FenceReq {
    type Msg = SessionToDaemonMsg;
    type ReplyMsg = SessionToClientMsg;
    type Reply = ();
    fn take_msg(&mut self) -> SessionToDaemonMsg {
        SessionToDaemonMsg::InputFence
    }

    fn interpret(&self, reply: SessionToClientMsg) -> Result<Self::Reply, ConnectError> {
        match reply {
            SessionToClientMsg::InputAccepted => Ok(()),
            _ => Err(ConnectError::NotSessionAttached),
        }
    }
}

struct ListReq;

impl CorrelatedRequest for ListReq {}

impl ControlRequest for ListReq {
    type Msg = OpsToDaemonMsg;
    type ReplyMsg = OpsToClientMsg;
    type Reply = Vec<SessionInfo>;
    fn take_msg(&mut self) -> OpsToDaemonMsg {
        OpsToDaemonMsg::List
    }

    fn interpret(&self, reply: OpsToClientMsg) -> Result<Self::Reply, ConnectError> {
        match reply {
            OpsToClientMsg::Listed { sessions } => Ok(sessions),
            _ => Err(ConnectError::NotSessionAttached),
        }
    }
}

struct DestroySessionReq {
    id_prefix: String,
}

impl CorrelatedRequest for DestroySessionReq {}

impl ControlRequest for DestroySessionReq {
    type Msg = OpsToDaemonMsg;
    type ReplyMsg = OpsToClientMsg;
    type Reply = ResolvedId;
    fn take_msg(&mut self) -> OpsToDaemonMsg {
        OpsToDaemonMsg::Destroy {
            id_prefix: std::mem::take(&mut self.id_prefix),
        }
    }

    fn interpret(&self, reply: OpsToClientMsg) -> Result<Self::Reply, ConnectError> {
        match reply {
            OpsToClientMsg::Destroyed { resolved } => Ok(resolved),
            _ => Err(ConnectError::NotSessionAttached),
        }
    }
}

pub(super) struct ForceDetachReq {
    pub(super) id_prefix: String,
}

impl CorrelatedRequest for ForceDetachReq {}

impl ControlRequest for ForceDetachReq {
    type Msg = OpsToDaemonMsg;
    type ReplyMsg = OpsToClientMsg;
    type Reply = (ResolvedId, bool);
    fn take_msg(&mut self) -> OpsToDaemonMsg {
        OpsToDaemonMsg::ForceDetach {
            id_prefix: std::mem::take(&mut self.id_prefix),
        }
    }

    fn interpret(&self, reply: OpsToClientMsg) -> Result<Self::Reply, ConnectError> {
        match reply {
            OpsToClientMsg::Detached {
                resolved,
                was_attached,
            } => Ok((resolved, was_attached)),
            _ => Err(ConnectError::NotSessionAttached),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchReply {
    pub from: ResolvedId,
    /// `None` for a carrier retarget: the daemon cannot resolve another
    /// daemon's roster.
    pub to: Option<ResolvedId>,
    /// Windows that took the push, not the ones that finished landing.
    pub queued: u32,
    pub denied: Option<SwitchDenied>,
}

pub(super) struct SwitchSessionReq {
    pub(super) from_prefix: String,
    pub(super) target: SwitchTarget,
    pub(super) scope: SwitchScope,
}

impl CorrelatedRequest for SwitchSessionReq {}

impl ControlRequest for SwitchSessionReq {
    type Msg = OpsToDaemonMsg;
    type ReplyMsg = OpsToClientMsg;
    type Reply = SwitchReply;
    fn take_msg(&mut self) -> OpsToDaemonMsg {
        OpsToDaemonMsg::Switch {
            from_prefix: std::mem::take(&mut self.from_prefix),
            target: self.target.clone(),
            scope: self.scope,
        }
    }

    fn interpret(&self, reply: OpsToClientMsg) -> Result<Self::Reply, ConnectError> {
        match reply {
            OpsToClientMsg::Switched {
                from,
                to,
                queued,
                denied,
            } => Ok(SwitchReply {
                from,
                to,
                queued,
                denied,
            }),
            _ => Err(ConnectError::NotSessionAttached),
        }
    }
}

/// What `felis daemon status` reports. Composed, not received whole:
/// the identity half is the handshake's
/// (`docs/explanation/architecture/ipc.md`), the live half the reply's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonStatus {
    /// `<semver> (<hash>)`.
    pub version: String,
    pub protocol_major: u16,
    /// The daemon's own minor, not this connection's effective minor.
    pub protocol_minor: u16,
    pub resources: Vec<ResourceReport>,
    pub worker_threads: u32,
    /// The daemon refuses creates and exits after its last session ends.
    pub draining: bool,
}

/// The reply's own half of [`DaemonStatus`]: what the daemon is holding
/// right now. Its identity half comes off the connection.
struct StatusLive {
    resources: Vec<ResourceReport>,
    worker_threads: u32,
    draining: bool,
}

struct StatusReq;

impl CorrelatedRequest for StatusReq {}

impl ControlRequest for StatusReq {
    type Msg = OpsToDaemonMsg;
    type ReplyMsg = OpsToClientMsg;
    type Reply = StatusLive;
    fn take_msg(&mut self) -> OpsToDaemonMsg {
        OpsToDaemonMsg::Status
    }

    fn interpret(&self, reply: OpsToClientMsg) -> Result<Self::Reply, ConnectError> {
        match reply {
            OpsToClientMsg::StatusReply {
                resources,
                worker_threads,
                draining,
            } => Ok(StatusLive {
                resources,
                worker_threads,
                draining,
            }),
            _ => Err(ConnectError::NotSessionAttached),
        }
    }
}

struct StopReq {
    mode: StopMode,
}

impl CorrelatedRequest for StopReq {}

impl ControlRequest for StopReq {
    type Msg = OpsToDaemonMsg;
    type ReplyMsg = OpsToClientMsg;
    type Reply = StopOutcome;
    fn take_msg(&mut self) -> OpsToDaemonMsg {
        OpsToDaemonMsg::Stop { mode: self.mode }
    }

    fn interpret(&self, reply: OpsToClientMsg) -> Result<Self::Reply, ConnectError> {
        match reply {
            OpsToClientMsg::StopReply { outcome } => Ok(outcome),
            _ => Err(ConnectError::NotSessionAttached),
        }
    }
}

struct UpgradeReq {
    successor: String,
}

impl CorrelatedRequest for UpgradeReq {}

impl ControlRequest for UpgradeReq {
    type Msg = OpsToDaemonMsg;
    type ReplyMsg = OpsToClientMsg;
    type Reply = UpgradeOutcome;
    fn take_msg(&mut self) -> OpsToDaemonMsg {
        OpsToDaemonMsg::Upgrade {
            successor: std::mem::take(&mut self.successor),
        }
    }

    fn interpret(&self, reply: OpsToClientMsg) -> Result<Self::Reply, ConnectError> {
        match reply {
            OpsToClientMsg::UpgradeReply { outcome } => Ok(outcome),
            _ => Err(ConnectError::NotSessionAttached),
        }
    }
}

pub(super) struct TagReq {
    pub(super) id_prefix: String,
    pub(super) add: Vec<String>,
    pub(super) remove: Vec<String>,
}

impl CorrelatedRequest for TagReq {}

impl ControlRequest for TagReq {
    type Msg = OpsToDaemonMsg;
    type ReplyMsg = OpsToClientMsg;
    type Reply = (ResolvedId, Vec<String>, Option<String>);
    fn take_msg(&mut self) -> OpsToDaemonMsg {
        OpsToDaemonMsg::Tag {
            id_prefix: std::mem::take(&mut self.id_prefix),
            add: std::mem::take(&mut self.add),
            remove: std::mem::take(&mut self.remove),
        }
    }

    fn interpret(&self, reply: OpsToClientMsg) -> Result<Self::Reply, ConnectError> {
        match reply {
            OpsToClientMsg::TagsUpdated {
                resolved,
                tags,
                denied,
            } => Ok((resolved, tags, denied)),
            _ => Err(ConnectError::NotSessionAttached),
        }
    }
}

impl<R, W> Connection<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Subscribe an observer connection to the notification stream,
    /// moving it from `Setup` to `Observing` as the opener goes out.
    ///
    /// # Errors
    /// As [`Self::open_stream`].
    pub async fn subscribe_notifications(
        &mut self,
        session_prefix: Option<String>,
    ) -> Result<StreamId, ConnectError> {
        self.open_stream_from(
            OpenFrom::Setup,
            &NotifyToDaemonMsg::Subscribe { session_prefix },
        )
        .await
    }

    pub async fn list_sessions(&mut self) -> Result<Vec<SessionInfo>, ConnectError> {
        self.correlated_request(ListReq).await
    }

    /// Resolves once the session processed every `Input` frame sent on
    /// this connection before it; it says nothing about the child
    /// having read those bytes.
    pub async fn input_fence(&mut self) -> Result<(), ConnectError> {
        self.correlated_request(FenceReq).await
    }

    /// The rehydrate burst follows on the same reader.
    pub async fn attach(
        &mut self,
        id: u128,
        intent: AttachIntent,
    ) -> Result<SessionInfo, ConnectError> {
        self.attach_to(AttachTarget::Id(id), intent).await
    }

    /// Attach by prefix, resolved daemon-side.
    ///
    /// # Errors
    /// [`ConnectError::InvalidSessionPrefix`], or [`ConnectError::AttachFailed`].
    pub async fn attach_by_prefix(
        &mut self,
        id_prefix: String,
        intent: AttachIntent,
    ) -> Result<SessionInfo, ConnectError> {
        let id_prefix = felis_protocol::session_prefix::validate_session_id_prefix(&id_prefix)
            .map_err(|reason| ConnectError::InvalidSessionPrefix {
                prefix: id_prefix.clone(),
                reason,
            })?;
        self.attach_to(AttachTarget::Prefix(id_prefix), intent)
            .await
    }

    async fn attach_to(
        &mut self,
        target: AttachTarget,
        intent: AttachIntent,
    ) -> Result<SessionInfo, ConnectError> {
        let info = self
            .request(AttachSessionReq {
                target,
                live_only: intent.live_only(),
            })
            .await?;
        self.driver.attached();
        Ok(info)
    }

    /// One session's roster row and its display prefix in one reply.
    ///
    /// # Errors
    /// [`ConnectError::MinorTooOld`] below the arm's own minor, or
    /// connection errors.
    pub async fn session_info(&mut self, id_prefix: String) -> Result<InfoOutcome, ConnectError> {
        check_arm_minor(
            &OpsToDaemonMsg::Info {
                id_prefix: String::new(),
            },
            self.effective_minor,
            "reading one session's roster row by prefix",
        )?;
        self.correlated_request(InfoReq { id_prefix }).await
    }

    /// Creates and subscribes a session atomically in one round trip.
    ///
    /// # Errors
    /// Connection or refusal errors.
    pub async fn create_with(&mut self, args: SpawnArgs) -> Result<SessionInfo, ConnectError> {
        let info = self.request(CreateSessionReq { args }).await?;
        // On the ack, not on the request: the rehydrate burst rides the
        // same reader, and a `Grid` frame classified while the driver
        // still says `Setup` would end the connection.
        self.driver.attached();
        Ok(info)
    }

    /// Creates a session without attaching this connection to it.
    ///
    /// # Errors
    /// Connection or refusal errors.
    pub async fn spawn_session(&mut self, args: SpawnArgs) -> Result<SessionInfo, ConnectError> {
        self.correlated_request(SpawnSessionReq { args }).await
    }

    pub async fn destroy_session(&mut self, id_prefix: String) -> Result<ResolvedId, ConnectError> {
        self.correlated_request(DestroySessionReq { id_prefix })
            .await
    }

    pub async fn force_detach(
        &mut self,
        id_prefix: String,
    ) -> Result<(ResolvedId, bool), ConnectError> {
        self.correlated_request(ForceDetachReq { id_prefix }).await
    }

    pub async fn switch_session(
        &mut self,
        from_prefix: String,
        to_prefix: String,
        scope: SwitchScope,
    ) -> Result<SwitchReply, ConnectError> {
        self.correlated_request(SwitchSessionReq {
            from_prefix,
            target: SwitchTarget::Session(to_prefix),
            scope,
        })
        .await
    }

    pub async fn retarget_window(
        &mut self,
        from_prefix: String,
        target: RetargetTarget,
        scope: SwitchScope,
    ) -> Result<SwitchReply, ConnectError> {
        self.correlated_request(SwitchSessionReq {
            from_prefix,
            target: SwitchTarget::Carrier(target),
            scope,
        })
        .await
    }

    pub async fn set_tags(
        &mut self,
        id_prefix: String,
        add: Vec<String>,
        remove: Vec<String>,
    ) -> Result<(ResolvedId, Vec<String>, Option<String>), ConnectError> {
        self.correlated_request(TagReq {
            id_prefix,
            add,
            remove,
        })
        .await
    }

    /// Gated rather than sent optimistically: an older daemon cannot
    /// decode the `Status` oneof arm and drops the connection over it.
    pub async fn daemon_status(&mut self) -> Result<DaemonStatus, ConnectError> {
        check_arm_minor(
            &OpsToDaemonMsg::Status,
            self.effective_minor,
            "reporting the daemon's resource accounting",
        )?;
        let live = self.correlated_request(StatusReq).await?;
        Ok(DaemonStatus {
            version: self
                .daemon_identity
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            protocol_major: self.accepted_major,
            protocol_minor: self.daemon_minor,
            resources: live.resources,
            worker_threads: live.worker_threads,
            draining: live.draining,
        })
    }

    /// Gated as [`Self::daemon_status`] is: a daemon below the stop
    /// minor decodes the arm as an empty oneof and drops the
    /// connection.
    pub async fn daemon_stop(&mut self, mode: StopMode) -> Result<StopOutcome, ConnectError> {
        check_arm_minor(
            &OpsToDaemonMsg::Stop { mode },
            self.effective_minor,
            "stopping the daemon",
        )?;
        self.correlated_request(StopReq { mode }).await
    }

    /// Gated as [`Self::daemon_stop`] is: a minor-0 daemon would drop
    /// the connection over the arm instead of refusing it.
    ///
    /// # Errors
    /// [`ConnectError::MinorTooOld`] below minor 1, with nothing sent.
    pub async fn daemon_upgrade(
        &mut self,
        successor: String,
    ) -> Result<UpgradeOutcome, ConnectError> {
        check_arm_minor(
            &OpsToDaemonMsg::Upgrade {
                successor: String::new(),
            },
            self.effective_minor,
            "upgrading the daemon in place",
        )?;
        self.correlated_request(UpgradeReq { successor }).await
    }
}

//! v1 DTO for [`SessionToDaemonMsg`] and [`SessionToClientMsg`], this
//! connection's session lifecycle.

use std::num::NonZeroU64;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use felis_protocol::codec::EitherHalf;
use felis_protocol::messages::{
    AttachFailure, AttachRefusal, AttachTarget, Attachment, CreateFailure, Directed as _,
    Notification, SessionInfo, SessionNotification, SessionToClientMsg, SessionToDaemonMsg,
    SpawnArgs, Urgency,
};

use super::JsonError;
#[cfg(feature = "schema")]
use super::common::SESSION_ID_PATTERN;
use super::common::{GridDimsJson, RequestedDimsJson, session_id_from_hex, session_id_hex};
use super::grid::plain_enum;

json_dto! {
    /// Desktop-notification urgency.
    #[serde(rename_all = "snake_case")]
    pub enum UrgencyJson {
        Low,
        Normal,
        Critical,
    }

    /// Why the daemon refused an attach or a create.
    #[serde(rename_all = "snake_case")]
    pub enum AttachFailureJson {
        UnknownSession,
        SessionEnding,
        SpawnFailed,
        GeometryOutOfRange,
        SessionExited,
        SessionLimitReached,
        DaemonDraining,
        NoMatch,
        Ambiguous,
    }

    /// Which session an attach names.
    #[serde(tag = "target", rename_all = "snake_case")]
    pub enum AttachTargetJson {
        Id {
            #[cfg_attr(feature = "schema", schemars(regex(pattern = SESSION_ID_PATTERN)))]
            id: String,
        },
        /// Lowercase hex prefix, 1–32 digits, resolved daemon-side.
        Prefix { prefix: String },
    }

    /// An instant on the daemon's clock, as seconds and nanoseconds
    /// since the Unix epoch.
    pub struct TimestampJson {
        pub unix_secs: u64,
        #[cfg_attr(feature = "schema", schemars(range(max = 999_999_999)))]
        pub unix_nanos: u32,
    }

    pub struct EnvPairJson {
        pub name: String,
        pub value: String,
    }

    /// One entry of a child's base environment, as raw platform bytes.
    pub struct EnvBytesJson {
        pub name: Vec<u8>,
        pub value: Vec<u8>,
    }

    pub struct SpawnArgsJson {
        /// Empty means the daemon's default (`$SHELL`).
        pub command: String,
        pub args: Vec<String>,
        /// Empty means the daemon's cwd.
        pub cwd: String,
        pub env: Vec<EnvPairJson>,
        /// `null` asks for the daemon default geometry.
        pub dims: Option<RequestedDimsJson>,
        pub env_base: Option<Vec<EnvBytesJson>>,
        pub tags: Vec<String>,
    }

    pub struct NotificationJson {
        pub title: Option<String>,
        pub body: String,
        pub urgency: UrgencyJson,
    }

    pub struct SessionNotificationJson {
        pub notification: NotificationJson,
        pub age_seconds: u64,
    }

    /// One live window attachment.
    pub struct AttachmentJson {
        pub id: u64,
        pub attached_at: TimestampJson,
        pub input_owner: bool,
    }

    /// One roster row.
    pub struct SessionInfoJson {
        /// The full 32-digit lowercase hex rendering.
        #[cfg_attr(feature = "schema", schemars(regex(pattern = SESSION_ID_PATTERN)))]
        pub id: String,
        pub dims: GridDimsJson,
        pub title: Option<String>,
        pub cwd: Option<String>,
        pub idle_seconds: Option<u64>,
        pub tags: Vec<String>,
        pub last_notification: Option<SessionNotificationJson>,
        pub foreground: Option<String>,
        pub exited: bool,
        pub last_exit_code: Option<u32>,
        pub attachments: Vec<AttachmentJson>,
        /// Creation sequence, minted by the daemon; never `0`.
        #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
        pub sequence: u64,
    }

    /// Session-family frames in their v1 form.
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum SessionJson {
        Attach {
            target: AttachTargetJson,
            live_only: bool,
        },
        Create {
            args: SpawnArgsJson,
        },
        Detach,
        ConfigureTheme {
            fg: Option<super::common::RgbJson>,
            bg: Option<super::common::RgbJson>,
            cursor: Option<super::common::RgbJson>,
        },
        Attached {
            info: Box<SessionInfoJson>,
        },
        AttachFailed {
            reason: AttachFailureJson,
            detail: String,
        },
        Created {
            info: Box<SessionInfoJson>,
        },
    }
}

plain_enum!(UrgencyJson, Urgency, Low, Normal, Critical);

/// Flattens the wire's two-arm refusal onto v1's one vocabulary: a
/// retype would be `felis_json: 2` (`docs/reference/ipc.md`
/// "Compatibility within a version"), and the two halves' token sets
/// are disjoint, so the arm is recovered from the token alone.
impl From<AttachRefusal> for AttachFailureJson {
    fn from(refusal: AttachRefusal) -> Self {
        match refusal {
            AttachRefusal::Attach(AttachFailure::UnknownSession) => Self::UnknownSession,
            AttachRefusal::Attach(AttachFailure::SessionEnding) => Self::SessionEnding,
            AttachRefusal::Attach(AttachFailure::SessionExited) => Self::SessionExited,
            AttachRefusal::Attach(AttachFailure::NoMatch) => Self::NoMatch,
            AttachRefusal::Attach(AttachFailure::Ambiguous) => Self::Ambiguous,
            AttachRefusal::Create(CreateFailure::SpawnFailed) => Self::SpawnFailed,
            AttachRefusal::Create(CreateFailure::GeometryOutOfRange) => Self::GeometryOutOfRange,
            AttachRefusal::Create(CreateFailure::SessionLimitReached) => Self::SessionLimitReached,
            AttachRefusal::Create(CreateFailure::DaemonDraining) => Self::DaemonDraining,
        }
    }
}

impl From<AttachFailureJson> for AttachRefusal {
    fn from(reason: AttachFailureJson) -> Self {
        match reason {
            AttachFailureJson::UnknownSession => Self::Attach(AttachFailure::UnknownSession),
            AttachFailureJson::SessionEnding => Self::Attach(AttachFailure::SessionEnding),
            AttachFailureJson::SessionExited => Self::Attach(AttachFailure::SessionExited),
            AttachFailureJson::NoMatch => Self::Attach(AttachFailure::NoMatch),
            AttachFailureJson::Ambiguous => Self::Attach(AttachFailure::Ambiguous),
            AttachFailureJson::SpawnFailed => Self::Create(CreateFailure::SpawnFailed),
            AttachFailureJson::GeometryOutOfRange => {
                Self::Create(CreateFailure::GeometryOutOfRange)
            }
            AttachFailureJson::SessionLimitReached => {
                Self::Create(CreateFailure::SessionLimitReached)
            }
            AttachFailureJson::DaemonDraining => Self::Create(CreateFailure::DaemonDraining),
        }
    }
}

impl From<AttachTarget> for AttachTargetJson {
    fn from(target: AttachTarget) -> Self {
        match target {
            AttachTarget::Id(id) => Self::Id {
                id: session_id_hex(id),
            },
            AttachTarget::Prefix(prefix) => Self::Prefix { prefix },
        }
    }
}

impl TryFrom<AttachTargetJson> for AttachTarget {
    type Error = JsonError;

    fn try_from(target: AttachTargetJson) -> Result<Self, Self::Error> {
        Ok(match target {
            AttachTargetJson::Id { id } => Self::Id(session_id_from_hex(&id)?),
            AttachTargetJson::Prefix { prefix } => Self::Prefix(prefix),
        })
    }
}

impl TryFrom<SystemTime> for TimestampJson {
    type Error = JsonError;

    fn try_from(time: SystemTime) -> Result<Self, Self::Error> {
        let since = time
            .duration_since(UNIX_EPOCH)
            .map_err(|_| JsonError::field("attached_at", "an instant before the Unix epoch"))?;
        Ok(Self {
            unix_secs: since.as_secs(),
            unix_nanos: since.subsec_nanos(),
        })
    }
}

impl TryFrom<TimestampJson> for SystemTime {
    type Error = JsonError;

    fn try_from(stamp: TimestampJson) -> Result<Self, JsonError> {
        if stamp.unix_nanos >= NANOS_PER_SEC {
            return Err(JsonError::field(
                "attached_at",
                "unix_nanos names a whole second or more",
            ));
        }
        UNIX_EPOCH
            .checked_add(Duration::new(stamp.unix_secs, stamp.unix_nanos))
            .ok_or_else(|| JsonError::field("attached_at", "an instant past this platform's clock"))
    }
}

const NANOS_PER_SEC: u32 = 1_000_000_000;

impl From<(String, String)> for EnvPairJson {
    fn from((name, value): (String, String)) -> Self {
        Self { name, value }
    }
}

impl From<EnvPairJson> for (String, String) {
    fn from(pair: EnvPairJson) -> Self {
        (pair.name, pair.value)
    }
}

impl From<(Vec<u8>, Vec<u8>)> for EnvBytesJson {
    fn from((name, value): (Vec<u8>, Vec<u8>)) -> Self {
        Self { name, value }
    }
}

impl From<EnvBytesJson> for (Vec<u8>, Vec<u8>) {
    fn from(pair: EnvBytesJson) -> Self {
        (pair.name, pair.value)
    }
}

impl From<SpawnArgs> for SpawnArgsJson {
    fn from(args: SpawnArgs) -> Self {
        Self {
            command: args.command,
            args: args.args,
            cwd: args.cwd,
            env: args.env.into_iter().map(Into::into).collect(),
            dims: args.dims.map(Into::into),
            env_base: args
                .env_base
                .map(|env| env.into_iter().map(Into::into).collect()),
            tags: args.tags,
        }
    }
}

impl From<SpawnArgsJson> for SpawnArgs {
    fn from(args: SpawnArgsJson) -> Self {
        Self {
            command: args.command,
            args: args.args,
            cwd: args.cwd,
            env: args.env.into_iter().map(Into::into).collect(),
            dims: args.dims.map(Into::into),
            env_base: args
                .env_base
                .map(|env| env.into_iter().map(Into::into).collect()),
            tags: args.tags,
        }
    }
}

impl From<Notification> for NotificationJson {
    fn from(notification: Notification) -> Self {
        Self {
            title: notification.title,
            body: notification.body,
            urgency: notification.urgency.into(),
        }
    }
}

impl From<NotificationJson> for Notification {
    fn from(notification: NotificationJson) -> Self {
        Self {
            title: notification.title,
            body: notification.body,
            urgency: notification.urgency.into(),
        }
    }
}

impl From<SessionNotification> for SessionNotificationJson {
    fn from(notification: SessionNotification) -> Self {
        Self {
            notification: notification.notification.into(),
            age_seconds: notification.age_seconds,
        }
    }
}

impl From<SessionNotificationJson> for SessionNotification {
    fn from(notification: SessionNotificationJson) -> Self {
        Self {
            notification: notification.notification.into(),
            age_seconds: notification.age_seconds,
        }
    }
}

impl TryFrom<Attachment> for AttachmentJson {
    type Error = JsonError;

    fn try_from(attachment: Attachment) -> Result<Self, Self::Error> {
        Ok(Self {
            id: attachment.id,
            attached_at: attachment.attached_at.try_into()?,
            input_owner: attachment.input_owner,
        })
    }
}

impl TryFrom<AttachmentJson> for Attachment {
    type Error = JsonError;

    fn try_from(attachment: AttachmentJson) -> Result<Self, JsonError> {
        Ok(Self {
            id: attachment.id,
            attached_at: attachment.attached_at.try_into()?,
            input_owner: attachment.input_owner,
        })
    }
}

impl TryFrom<SessionInfo> for SessionInfoJson {
    type Error = JsonError;

    fn try_from(info: SessionInfo) -> Result<Self, Self::Error> {
        Ok(Self {
            id: session_id_hex(info.id),
            dims: info.dims.into(),
            title: info.title,
            cwd: info.cwd,
            idle_seconds: info.idle_seconds,
            tags: info.tags,
            last_notification: info.last_notification.map(Into::into),
            foreground: info.foreground,
            exited: info.exited,
            last_exit_code: info.last_exit_code,
            attachments: info
                .attachments
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, JsonError>>()?,
            sequence: info.sequence.get(),
        })
    }
}

impl TryFrom<SessionInfoJson> for SessionInfo {
    type Error = JsonError;

    fn try_from(info: SessionInfoJson) -> Result<Self, Self::Error> {
        let sequence = NonZeroU64::new(info.sequence)
            .ok_or_else(|| JsonError::field("sequence", "a creation sequence is never `0`"))?;
        Ok(Self {
            id: session_id_from_hex(&info.id)?,
            dims: info.dims.into(),
            title: info.title,
            cwd: info.cwd,
            idle_seconds: info.idle_seconds,
            tags: info.tags,
            last_notification: info.last_notification.map(Into::into),
            foreground: info.foreground,
            exited: info.exited,
            last_exit_code: info.last_exit_code,
            attachments: info
                .attachments
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, JsonError>>()?,
            sequence,
        })
    }
}

impl TryFrom<SessionToDaemonMsg> for SessionJson {
    type Error = JsonError;

    fn try_from(msg: SessionToDaemonMsg) -> Result<Self, Self::Error> {
        Ok(match msg {
            SessionToDaemonMsg::Attach { target, live_only } => Self::Attach {
                target: target.into(),
                live_only,
            },
            SessionToDaemonMsg::Create { args } => Self::Create { args: args.into() },
            SessionToDaemonMsg::Detach => Self::Detach,
            SessionToDaemonMsg::ConfigureTheme { fg, bg, cursor } => Self::ConfigureTheme {
                fg: fg.map(Into::into),
                bg: bg.map(Into::into),
                cursor: cursor.map(Into::into),
            },
            // The fence pair is a correlated request/reply, and this
            // format carries no correlation envelope: a reply stripped
            // of the id it answers names no fence.
            ref outside @ SessionToDaemonMsg::InputFence => {
                return Err(JsonError::Outside(outside.variant()));
            }
        })
    }
}

impl TryFrom<SessionToClientMsg> for SessionJson {
    type Error = JsonError;

    fn try_from(msg: SessionToClientMsg) -> Result<Self, Self::Error> {
        Ok(match msg {
            SessionToClientMsg::Attached { info } => Self::Attached {
                info: Box::new(info.try_into()?),
            },
            SessionToClientMsg::AttachFailed { reason, detail } => Self::AttachFailed {
                reason: reason.into(),
                detail,
            },
            SessionToClientMsg::Created { info } => Self::Created {
                info: Box::new(info.try_into()?),
            },
            ref outside @ SessionToClientMsg::InputAccepted => {
                return Err(JsonError::Outside(outside.variant()));
            }
        })
    }
}

impl TryFrom<EitherHalf<SessionToDaemonMsg, SessionToClientMsg>> for SessionJson {
    type Error = JsonError;

    fn try_from(
        msg: EitherHalf<SessionToDaemonMsg, SessionToClientMsg>,
    ) -> Result<Self, Self::Error> {
        match msg {
            EitherHalf::ToDaemon(msg) => msg.try_into(),
            EitherHalf::ToClient(msg) => msg.try_into(),
        }
    }
}

impl TryFrom<SessionJson> for EitherHalf<SessionToDaemonMsg, SessionToClientMsg> {
    type Error = JsonError;

    fn try_from(msg: SessionJson) -> Result<Self, Self::Error> {
        Ok(match msg {
            SessionJson::Attach { target, live_only } => {
                Self::ToDaemon(SessionToDaemonMsg::Attach {
                    target: target.try_into()?,
                    live_only,
                })
            }
            SessionJson::Create { args } => {
                Self::ToDaemon(SessionToDaemonMsg::Create { args: args.into() })
            }
            SessionJson::Detach => Self::ToDaemon(SessionToDaemonMsg::Detach),
            SessionJson::ConfigureTheme { fg, bg, cursor } => {
                Self::ToDaemon(SessionToDaemonMsg::ConfigureTheme {
                    fg: fg.map(Into::into),
                    bg: bg.map(Into::into),
                    cursor: cursor.map(Into::into),
                })
            }
            SessionJson::Attached { info } => Self::ToClient(SessionToClientMsg::Attached {
                info: (*info).try_into()?,
            }),
            SessionJson::AttachFailed { reason, detail } => {
                Self::ToClient(SessionToClientMsg::AttachFailed {
                    reason: reason.into(),
                    detail,
                })
            }
            SessionJson::Created { info } => Self::ToClient(SessionToClientMsg::Created {
                info: (*info).try_into()?,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        JsonError, SessionJson, SessionToClientMsg, SessionToDaemonMsg, SystemTime, TimestampJson,
        UNIX_EPOCH,
    };
    use std::time::Duration;

    #[test]
    fn the_fence_pair_is_refused_as_outside_the_format() {
        assert!(matches!(
            SessionJson::try_from(SessionToDaemonMsg::InputFence),
            Err(JsonError::Outside(_))
        ));
        assert!(matches!(
            SessionJson::try_from(SessionToClientMsg::InputAccepted),
            Err(JsonError::Outside(_))
        ));
    }

    #[test]
    fn a_whole_second_of_nanoseconds_is_refused_rather_than_normalized() {
        let stamp = TimestampJson {
            unix_secs: 1_700_000_000,
            unix_nanos: 1_000_000_000,
        };
        assert!(matches!(
            SystemTime::try_from(stamp),
            Err(JsonError::Field {
                field: "attached_at",
                ..
            })
        ));
    }

    #[test]
    fn a_timestamp_past_the_platform_clock_is_refused_rather_than_panicking() {
        let stamp = TimestampJson {
            unix_secs: u64::MAX,
            unix_nanos: 0,
        };
        assert!(matches!(
            SystemTime::try_from(stamp),
            Err(JsonError::Field {
                field: "attached_at",
                ..
            })
        ));
    }

    #[test]
    fn a_timestamp_round_trips_to_the_same_instant() {
        let instant = UNIX_EPOCH + Duration::new(1_700_000_000, 999_999_999);
        let stamp = TimestampJson::try_from(instant).expect("an instant after the epoch converts");
        assert_eq!(stamp.unix_secs, 1_700_000_000);
        assert_eq!(stamp.unix_nanos, 999_999_999);
        assert_eq!(
            SystemTime::try_from(stamp).expect("the stamp converts back"),
            instant
        );
    }
}

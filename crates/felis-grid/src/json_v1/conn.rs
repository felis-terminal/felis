//! v1 DTO for [`ConnToDaemonMsg`] and [`ConnToClientMsg`], the handshake
//! and the stream lifecycle.

use felis_protocol::build_identity::BuildIdentity;
use felis_protocol::caps::ConnectionMode;
use felis_protocol::codec::EitherHalf;
use felis_protocol::messages::{
    ConnToClientMsg, ConnToDaemonMsg, RefusalReason, RequestId, StreamErrorReason, StreamId,
    Subject,
};

use super::JsonError;
use super::grid::plain_enum;

json_dto! {
    /// What a connection is allowed to say.
    #[serde(rename_all = "snake_case")]
    pub enum ConnectionModeJson {
        Window,
        Ops,
        Observer,
    }

    /// Why a daemon refused a connection.
    #[serde(rename_all = "snake_case")]
    pub enum RefusalReasonJson {
        Role,
        AtCapacity,
        UnknownMode,
    }

    /// Why a request or a stream failed.
    #[serde(rename_all = "snake_case")]
    pub enum StreamErrorReasonJson {
        InvalidRequest,
        TooManyStreams,
        Unavailable,
        Internal,
    }

    /// What a [`ConnJson::Error`] is about.
    #[serde(tag = "subject", rename_all = "snake_case")]
    pub enum SubjectJson {
        /// Correlation ids start at 1, so `0` names nothing.
        Request {
            #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
            id: u64,
        },
        Stream {
            #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
            id: u64,
        },
    }

    /// The build a daemon reports.
    pub struct BuildIdentityJson {
        pub version: String,
        pub revision: String,
        pub dirty: bool,
    }

    /// Connection-family frames in their v1 form.
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum ConnJson {
        Hello {
            mode: ConnectionModeJson,
            pull_paced: bool,
        },
        Welcome {
            identity: Option<BuildIdentityJson>,
        },
        Refused {
            reason: RefusalReasonJson,
            detail: String,
        },
        Cancel {
            #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
            stream_id: u64,
        },
        End {
            #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
            stream_id: u64,
            count: u32,
        },
        Error {
            subject: SubjectJson,
            reason: StreamErrorReasonJson,
            detail: String,
        },
    }
}

plain_enum!(ConnectionModeJson, ConnectionMode, Window, Ops, Observer);
plain_enum!(
    RefusalReasonJson,
    RefusalReason,
    Role,
    AtCapacity,
    UnknownMode
);
plain_enum!(
    StreamErrorReasonJson,
    StreamErrorReason,
    InvalidRequest,
    TooManyStreams,
    Unavailable,
    Internal
);

impl From<BuildIdentity> for BuildIdentityJson {
    fn from(identity: BuildIdentity) -> Self {
        Self {
            version: identity.version,
            revision: identity.revision,
            dirty: identity.dirty,
        }
    }
}

impl From<BuildIdentityJson> for BuildIdentity {
    fn from(identity: BuildIdentityJson) -> Self {
        Self {
            version: identity.version,
            revision: identity.revision,
            dirty: identity.dirty,
        }
    }
}

impl From<Subject> for SubjectJson {
    fn from(subject: Subject) -> Self {
        match subject {
            Subject::Request(id) => Self::Request { id: id.get() },
            Subject::Stream(id) => Self::Stream { id: id.get() },
        }
    }
}

impl TryFrom<SubjectJson> for Subject {
    type Error = JsonError;

    fn try_from(subject: SubjectJson) -> Result<Self, Self::Error> {
        Ok(match subject {
            SubjectJson::Request { id } => Self::Request(
                RequestId::new(id).ok_or_else(|| JsonError::field("id", "a request id is ≥ 1"))?,
            ),
            SubjectJson::Stream { id } => Self::Stream(stream_id(id)?),
        })
    }
}

fn stream_id(id: u64) -> Result<StreamId, JsonError> {
    StreamId::new(id).ok_or_else(|| JsonError::field("stream_id", "a stream id is ≥ 1"))
}

impl From<ConnToDaemonMsg> for ConnJson {
    fn from(msg: ConnToDaemonMsg) -> Self {
        match msg {
            ConnToDaemonMsg::Hello { mode, pull_paced } => Self::Hello {
                mode: mode.into(),
                pull_paced,
            },
            ConnToDaemonMsg::Cancel { stream_id } => Self::Cancel {
                stream_id: stream_id.get(),
            },
        }
    }
}

impl From<ConnToClientMsg> for ConnJson {
    fn from(msg: ConnToClientMsg) -> Self {
        match msg {
            ConnToClientMsg::Welcome { identity } => Self::Welcome {
                identity: identity.map(Into::into),
            },
            ConnToClientMsg::Refused { reason, detail } => Self::Refused {
                reason: reason.into(),
                detail,
            },
            ConnToClientMsg::End { stream_id, count } => Self::End {
                stream_id: stream_id.get(),
                count,
            },
            ConnToClientMsg::Error {
                subject,
                reason,
                detail,
            } => Self::Error {
                subject: subject.into(),
                reason: reason.into(),
                detail,
            },
        }
    }
}

impl From<EitherHalf<ConnToDaemonMsg, ConnToClientMsg>> for ConnJson {
    fn from(msg: EitherHalf<ConnToDaemonMsg, ConnToClientMsg>) -> Self {
        match msg {
            EitherHalf::ToDaemon(msg) => msg.into(),
            EitherHalf::ToClient(msg) => msg.into(),
        }
    }
}

impl TryFrom<ConnJson> for EitherHalf<ConnToDaemonMsg, ConnToClientMsg> {
    type Error = JsonError;

    fn try_from(msg: ConnJson) -> Result<Self, JsonError> {
        Ok(match msg {
            ConnJson::Hello { mode, pull_paced } => Self::ToDaemon(ConnToDaemonMsg::Hello {
                mode: mode.into(),
                pull_paced,
            }),
            ConnJson::Welcome { identity } => Self::ToClient(ConnToClientMsg::Welcome {
                identity: identity.map(Into::into),
            }),
            ConnJson::Refused { reason, detail } => Self::ToClient(ConnToClientMsg::Refused {
                reason: reason.into(),
                detail,
            }),
            ConnJson::Cancel { stream_id: id } => Self::ToDaemon(ConnToDaemonMsg::Cancel {
                stream_id: stream_id(id)?,
            }),
            ConnJson::End {
                stream_id: id,
                count,
            } => Self::ToClient(ConnToClientMsg::End {
                stream_id: stream_id(id)?,
                count,
            }),
            ConnJson::Error {
                subject,
                reason,
                detail,
            } => Self::ToClient(ConnToClientMsg::Error {
                subject: subject.try_into()?,
                reason: reason.into(),
                detail,
            }),
        })
    }
}

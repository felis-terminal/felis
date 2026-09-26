//! `ConnToDaemonMsg` / `ConnToClientMsg` <-> wire, plus the `ConnectionMode` bridge the handshake
//! rides.

use super::{WireError, decode_enum};
use crate::build_identity::BuildIdentity;
use crate::caps::ConnectionMode;
use crate::messages;
use crate::messages::RefusalReason;
use crate::wire::v1;

const fn mode_to_wire(m: ConnectionMode) -> v1::ConnectionMode {
    match m {
        ConnectionMode::Window => v1::ConnectionMode::Window,
        ConnectionMode::Ops => v1::ConnectionMode::Ops,
        ConnectionMode::Observer => v1::ConnectionMode::Observer,
    }
}

/// `UNSPECIFIED` is a hard error: defaulting the mode would wire up a
/// surface the peer never named.
fn mode_from_wire(raw: i32) -> Result<ConnectionMode, WireError> {
    match decode_enum::<v1::ConnectionMode>("ConnectionMode", raw)? {
        v1::ConnectionMode::Unspecified => Err(WireError::UnspecifiedEnum("ConnectionMode")),
        v1::ConnectionMode::Window => Ok(ConnectionMode::Window),
        v1::ConnectionMode::Ops => Ok(ConnectionMode::Ops),
        v1::ConnectionMode::Observer => Ok(ConnectionMode::Observer),
    }
}

const fn refusal_to_wire(r: RefusalReason) -> v1::RefusalReason {
    match r {
        RefusalReason::Role => v1::RefusalReason::Role,
        RefusalReason::AtCapacity => v1::RefusalReason::AtCapacity,
        RefusalReason::UnknownMode => v1::RefusalReason::UnknownMode,
    }
}

fn refusal_from_wire(raw: i32) -> Result<RefusalReason, WireError> {
    match decode_enum::<v1::RefusalReason>("RefusalReason", raw)? {
        v1::RefusalReason::Unspecified => Err(WireError::UnspecifiedEnum("RefusalReason")),
        v1::RefusalReason::Role => Ok(RefusalReason::Role),
        v1::RefusalReason::AtCapacity => Ok(RefusalReason::AtCapacity),
        v1::RefusalReason::UnknownMode => Ok(RefusalReason::UnknownMode),
    }
}

const fn stream_error_to_wire(r: messages::StreamErrorReason) -> v1::StreamErrorReason {
    use messages::StreamErrorReason as R;
    match r {
        R::InvalidRequest => v1::StreamErrorReason::InvalidRequest,
        R::TooManyStreams => v1::StreamErrorReason::TooManyStreams,
        R::Unavailable => v1::StreamErrorReason::Unavailable,
        R::Internal => v1::StreamErrorReason::Internal,
    }
}

/// `UNSPECIFIED` is refused: a caller picks its remedy from the reason.
fn stream_error_from_wire(raw: i32) -> Result<messages::StreamErrorReason, WireError> {
    use messages::StreamErrorReason as R;
    match decode_enum::<v1::StreamErrorReason>("StreamErrorReason", raw)? {
        v1::StreamErrorReason::Unspecified => Err(WireError::UnspecifiedEnum("StreamErrorReason")),
        v1::StreamErrorReason::InvalidRequest => Ok(R::InvalidRequest),
        v1::StreamErrorReason::TooManyStreams => Ok(R::TooManyStreams),
        v1::StreamErrorReason::Unavailable => Ok(R::Unavailable),
        v1::StreamErrorReason::Internal => Ok(R::Internal),
    }
}

impl From<&messages::ConnToDaemonMsg> for v1::ConnToDaemonMsg {
    fn from(m: &messages::ConnToDaemonMsg) -> Self {
        use messages::ConnToDaemonMsg as C;
        use v1::conn_to_daemon_msg::Msg;
        let msg = match m {
            C::Hello { mode, pull_paced } => Msg::Hello(v1::ConnHello {
                mode: mode_to_wire(*mode) as i32,
                pull_paced: *pull_paced,
            }),
            C::Cancel { stream_id } => Msg::Cancel(v1::ConnCancel {
                stream_id: stream_id.get(),
            }),
        };
        Self { msg: Some(msg) }
    }
}

impl TryFrom<v1::ConnToDaemonMsg> for messages::ConnToDaemonMsg {
    type Error = WireError;
    fn try_from(m: v1::ConnToDaemonMsg) -> Result<Self, Self::Error> {
        use v1::conn_to_daemon_msg::Msg;
        Ok(
            match m
                .msg
                .ok_or(WireError::MissingOneof("ConnToDaemonMsg.msg"))?
            {
                Msg::Hello(h) => Self::Hello {
                    mode: mode_from_wire(h.mode)?,
                    pull_paced: h.pull_paced,
                },
                Msg::Cancel(c) => Self::Cancel {
                    stream_id: super::stream_id_from_wire("ConnCancel.stream_id", c.stream_id)?,
                },
            },
        )
    }
}

impl From<&messages::ConnToClientMsg> for v1::ConnToClientMsg {
    fn from(m: &messages::ConnToClientMsg) -> Self {
        use messages::ConnToClientMsg as C;
        use v1::conn_to_client_msg::Msg;
        let msg = match m {
            C::Welcome { identity } => Msg::Welcome(v1::ConnWelcome {
                identity: identity.as_ref().map(Into::into),
            }),
            C::Refused { reason, detail } => Msg::Refused(v1::ConnRefused {
                reason: refusal_to_wire(*reason) as i32,
                detail: detail.clone(),
            }),
            C::End { stream_id, count } => Msg::End(v1::ConnEnd {
                stream_id: stream_id.get(),
                count: *count,
            }),
            C::Error {
                subject,
                reason,
                detail,
            } => Msg::Error(v1::ConnError {
                subject: Some(match subject {
                    messages::Subject::Request(id) => v1::conn_error::Subject::RequestId(id.get()),
                    messages::Subject::Stream(id) => v1::conn_error::Subject::StreamId(id.get()),
                }),
                reason: stream_error_to_wire(*reason) as i32,
                detail: detail.clone(),
            }),
        };
        Self { msg: Some(msg) }
    }
}

impl TryFrom<v1::ConnToClientMsg> for messages::ConnToClientMsg {
    type Error = WireError;
    // Spelled out rather than `Self::Error`: `ConnToClientMsg` has an
    // `Error` *variant*, which makes the associated-item path ambiguous.
    fn try_from(m: v1::ConnToClientMsg) -> Result<Self, WireError> {
        use v1::conn_to_client_msg::Msg;
        Ok(
            match m
                .msg
                .ok_or(WireError::MissingOneof("ConnToClientMsg.msg"))?
            {
                Msg::Welcome(w) => Self::Welcome {
                    identity: w.identity.map(Into::into),
                },
                Msg::Refused(r) => Self::Refused {
                    reason: refusal_from_wire(r.reason)?,
                    detail: r.detail,
                },
                Msg::End(e) => Self::End {
                    stream_id: super::stream_id_from_wire("ConnEnd.stream_id", e.stream_id)?,
                    count: e.count,
                },
                Msg::Error(e) => Self::Error {
                    subject: match e
                        .subject
                        .ok_or(WireError::MissingOneof("ConnError.subject"))?
                    {
                        v1::conn_error::Subject::RequestId(id) => messages::Subject::Request(
                            super::request_id_from_wire("ConnError.request_id", id)?,
                        ),
                        v1::conn_error::Subject::StreamId(id) => messages::Subject::Stream(
                            super::stream_id_from_wire("ConnError.stream_id", id)?,
                        ),
                    },
                    reason: stream_error_from_wire(e.reason)?,
                    detail: e.detail,
                },
            },
        )
    }
}

impl From<&BuildIdentity> for v1::BuildIdentity {
    fn from(id: &BuildIdentity) -> Self {
        Self {
            version: id.version.clone(),
            revision: id.revision.clone(),
            dirty: id.dirty,
        }
    }
}

/// Infallible: an identity is a report, never a gate, so refusing a
/// nonsense one would cost the whole connection over a display field.
impl From<v1::BuildIdentity> for BuildIdentity {
    fn from(id: v1::BuildIdentity) -> Self {
        Self {
            version: id.version,
            revision: id.revision,
            dirty: id.dirty,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::{ConnToClientMsg, ConnToDaemonMsg};

    #[test]
    fn missing_oneof_is_rejected() {
        assert!(matches!(
            ConnToDaemonMsg::try_from(v1::ConnToDaemonMsg { msg: None }),
            Err(WireError::MissingOneof("ConnToDaemonMsg.msg"))
        ));
        assert!(matches!(
            ConnToClientMsg::try_from(v1::ConnToClientMsg { msg: None }),
            Err(WireError::MissingOneof("ConnToClientMsg.msg"))
        ));
    }

    /// A `Hello` whose mode is absent or unknown fails the decode
    /// rather than defaulting.
    #[test]
    fn a_hello_without_a_readable_mode_is_fatal() {
        let hello = |mode: i32| v1::ConnToDaemonMsg {
            msg: Some(v1::conn_to_daemon_msg::Msg::Hello(v1::ConnHello {
                mode,
                pull_paced: false,
            })),
        };
        assert!(matches!(
            ConnToDaemonMsg::try_from(hello(v1::ConnectionMode::Unspecified as i32)),
            Err(WireError::UnspecifiedEnum("ConnectionMode"))
        ));
        assert!(matches!(
            ConnToDaemonMsg::try_from(hello(99)),
            Err(WireError::UnknownEnum {
                field: "ConnectionMode",
                value: 99
            })
        ));
    }

    /// A refusal value this build does not define is rejected, not
    /// read as the nearest one it does.
    #[test]
    fn an_undefined_refusal_value_is_rejected() {
        for undefined in [4, 99] {
            let refused = v1::ConnToClientMsg {
                msg: Some(v1::conn_to_client_msg::Msg::Refused(v1::ConnRefused {
                    reason: undefined,
                    detail: String::new(),
                })),
            };
            assert!(matches!(
                ConnToClientMsg::try_from(refused),
                Err(WireError::UnknownEnum {
                    field: "RefusalReason",
                    ..
                })
            ));
        }
    }
}

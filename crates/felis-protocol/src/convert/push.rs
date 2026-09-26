//! `PushMsg` <-> wire.

use super::{WireError, id_from_bytes, id_to_bytes};
use crate::messages;
use crate::wire::v1;

impl From<&messages::PushMsg> for v1::PushMsg {
    fn from(m: &messages::PushMsg) -> Self {
        use messages::PushMsg as P;
        use v1::push_msg::Msg;
        let msg = match m {
            P::Evicted { reason } => Msg::Evicted(v1::PushEvicted {
                reason: reason.clone(),
            }),
            P::Reattach { id } => Msg::Reattach(v1::PushReattach {
                id: id_to_bytes(*id),
            }),
            P::SessionExited { id } => Msg::SessionExited(v1::PushSessionExited {
                id: id_to_bytes(*id),
            }),
            P::RetargetHost { target } => Msg::RetargetHost(v1::PushRetargetHost {
                target: Some(target.into()),
            }),
        };
        Self { msg: Some(msg) }
    }
}

impl TryFrom<v1::PushMsg> for messages::PushMsg {
    type Error = WireError;
    fn try_from(m: v1::PushMsg) -> Result<Self, Self::Error> {
        use v1::push_msg::Msg;
        Ok(match m.msg.ok_or(WireError::MissingOneof("PushMsg.msg"))? {
            Msg::Evicted(e) => Self::Evicted { reason: e.reason },
            Msg::Reattach(r) => Self::Reattach {
                id: id_from_bytes(r.id)?,
            },
            Msg::SessionExited(s) => Self::SessionExited {
                id: id_from_bytes(s.id)?,
            },
            Msg::RetargetHost(r) => Self::RetargetHost {
                target: r
                    .target
                    .ok_or(WireError::MissingField("RetargetHost.target"))?
                    .try_into()?,
            },
        })
    }
}

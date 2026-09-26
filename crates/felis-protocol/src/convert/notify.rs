//! `NotifyToDaemonMsg` / `NotifyToClientMsg` <-> wire.

use super::{WireError, id_from_bytes, id_to_bytes};
use crate::messages;
use crate::wire::v1;

impl From<&messages::NotifyToDaemonMsg> for v1::NotifyToDaemonMsg {
    fn from(m: &messages::NotifyToDaemonMsg) -> Self {
        use messages::NotifyToDaemonMsg as N;
        use v1::notify_to_daemon_msg::Msg;
        let msg = match m {
            N::Subscribe { session_prefix } => Msg::Subscribe(v1::NotifySubscribe {
                session_prefix: session_prefix.clone(),
            }),
        };
        Self {
            msg: Some(msg),
            correlation: None,
        }
    }
}

impl From<&messages::NotifyToClientMsg> for v1::NotifyToClientMsg {
    fn from(m: &messages::NotifyToClientMsg) -> Self {
        use messages::NotifyToClientMsg as N;
        use v1::notify_to_client_msg::Msg;
        let msg = match m {
            N::Subscribed { filter } => Msg::Subscribed(v1::NotifySubscribed {
                filter: filter.map(Into::into),
            }),
            N::Event {
                session_id,
                notification,
                notify_id,
                session_title,
                cwd,
                attached,
            } => Msg::Event(v1::NotifyEvent {
                session_id: id_to_bytes(*session_id),
                notification: Some(notification.into()),
                notify_id: notify_id.clone(),
                session_title: session_title.clone(),
                cwd: cwd.clone(),
                attached: *attached,
            }),
            N::Lagged { missed } => Msg::Lagged(v1::NotifyLagged { missed: *missed }),
        };
        Self {
            msg: Some(msg),
            correlation: None,
        }
    }
}

impl TryFrom<v1::NotifyToDaemonMsg> for messages::NotifyToDaemonMsg {
    type Error = WireError;
    fn try_from(m: v1::NotifyToDaemonMsg) -> Result<Self, Self::Error> {
        use v1::notify_to_daemon_msg::Msg;
        Ok(
            match m
                .msg
                .ok_or(WireError::MissingOneof("NotifyToDaemonMsg.msg"))?
            {
                Msg::Subscribe(sub) => Self::Subscribe {
                    session_prefix: sub.session_prefix,
                },
            },
        )
    }
}

impl TryFrom<v1::NotifyToClientMsg> for messages::NotifyToClientMsg {
    type Error = WireError;
    fn try_from(m: v1::NotifyToClientMsg) -> Result<Self, Self::Error> {
        use v1::notify_to_client_msg::Msg;
        Ok(
            match m
                .msg
                .ok_or(WireError::MissingOneof("NotifyToClientMsg.msg"))?
            {
                Msg::Subscribed(sub) => Self::Subscribed {
                    filter: sub.filter.map(TryInto::try_into).transpose()?,
                },
                Msg::Event(e) => Self::Event {
                    session_id: id_from_bytes(e.session_id)?,
                    notification: messages::Notification::try_from(
                        e.notification
                            .ok_or(WireError::MissingField("Event.notification"))?,
                    )?,
                    notify_id: e.notify_id,
                    session_title: e.session_title,
                    cwd: e.cwd,
                    attached: e.attached,
                },
                Msg::Lagged(l) => Self::Lagged { missed: l.missed },
            },
        )
    }
}

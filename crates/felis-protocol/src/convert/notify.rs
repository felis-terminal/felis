//! `NotifyToDaemonMsg` / `NotifyToClientMsg` <-> wire.

use super::{WireError, decode_enum, id_from_bytes, id_to_bytes};
use crate::messages;
use crate::wire::v1;

const fn observer_kind_to_wire(k: messages::ObserverKind) -> v1::ObserverKind {
    use messages::ObserverKind as K;
    match k {
        K::Notification => v1::ObserverKind::Notification,
        K::Lifecycle => v1::ObserverKind::Lifecycle,
        K::ProgramStatus => v1::ObserverKind::ProgramStatus,
        K::Prompt => v1::ObserverKind::Prompt,
        K::Meta => v1::ObserverKind::Meta,
    }
}

fn observer_kind_from_wire(raw: i32) -> Result<messages::ObserverKind, WireError> {
    use messages::ObserverKind as K;
    match decode_enum::<v1::ObserverKind>("NotifySubscribe.kinds", raw)? {
        v1::ObserverKind::Unspecified => Err(WireError::UnspecifiedEnum("ObserverKind")),
        v1::ObserverKind::Notification => Ok(K::Notification),
        v1::ObserverKind::Lifecycle => Ok(K::Lifecycle),
        v1::ObserverKind::ProgramStatus => Ok(K::ProgramStatus),
        v1::ObserverKind::Prompt => Ok(K::Prompt),
        v1::ObserverKind::Meta => Ok(K::Meta),
    }
}

const fn session_field_to_wire(f: messages::SessionField) -> v1::SessionField {
    use messages::SessionField as F;
    match f {
        F::Tags => v1::SessionField::Tags,
        F::Title => v1::SessionField::Title,
        F::Cwd => v1::SessionField::Cwd,
        F::Attachments => v1::SessionField::Attachments,
    }
}

fn session_field_from_wire(raw: i32) -> Result<messages::SessionField, WireError> {
    use messages::SessionField as F;
    match decode_enum::<v1::SessionField>("NotifySessionChanged.fields", raw)? {
        v1::SessionField::Unspecified => Err(WireError::UnspecifiedEnum("SessionField")),
        v1::SessionField::Tags => Ok(F::Tags),
        v1::SessionField::Title => Ok(F::Title),
        v1::SessionField::Cwd => Ok(F::Cwd),
        v1::SessionField::Attachments => Ok(F::Attachments),
    }
}

impl From<&messages::NotifyToDaemonMsg> for v1::NotifyToDaemonMsg {
    fn from(m: &messages::NotifyToDaemonMsg) -> Self {
        use messages::NotifyToDaemonMsg as N;
        use v1::notify_to_daemon_msg::Msg;
        let msg = match m {
            N::Subscribe {
                session_prefix,
                kinds,
            } => Msg::Subscribe(v1::NotifySubscribe {
                session_prefix: session_prefix.clone(),
                kinds: kinds
                    .iter()
                    .map(|k| observer_kind_to_wire(*k) as i32)
                    .collect(),
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
            N::SessionAdded { session_id } => Msg::SessionAdded(v1::NotifySessionAdded {
                session_id: id_to_bytes(*session_id),
            }),
            N::SessionExited { session_id, status } => {
                Msg::SessionExited(v1::NotifySessionExited {
                    session_id: id_to_bytes(*session_id),
                    status: status.map(Into::into),
                })
            }
            N::SessionRemoved { session_id } => Msg::SessionRemoved(v1::NotifySessionRemoved {
                session_id: id_to_bytes(*session_id),
            }),
            N::ProgramStatus {
                session_id,
                records,
            } => Msg::ProgramStatus(v1::NotifyProgramStatus {
                session_id: id_to_bytes(*session_id),
                records: records.iter().map(Into::into).collect(),
            }),
            N::PromptShown { session_id } => Msg::PromptShown(v1::NotifyPromptShown {
                session_id: id_to_bytes(*session_id),
            }),
            N::CommandFinished {
                session_id,
                exit_code,
            } => Msg::CommandFinished(v1::NotifyCommandFinished {
                session_id: id_to_bytes(*session_id),
                exit_code: *exit_code,
            }),
            N::SessionChanged { session_id, fields } => {
                Msg::SessionChanged(v1::NotifySessionChanged {
                    session_id: id_to_bytes(*session_id),
                    fields: fields
                        .iter()
                        .map(|f| session_field_to_wire(*f) as i32)
                        .collect(),
                })
            }
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
                    kinds: sub
                        .kinds
                        .into_iter()
                        .map(observer_kind_from_wire)
                        .collect::<Result<_, _>>()?,
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
                Msg::SessionAdded(a) => Self::SessionAdded {
                    session_id: id_from_bytes(a.session_id)?,
                },
                Msg::SessionExited(e) => Self::SessionExited {
                    session_id: id_from_bytes(e.session_id)?,
                    status: e.status.map(TryInto::try_into).transpose()?,
                },
                Msg::SessionRemoved(r) => Self::SessionRemoved {
                    session_id: id_from_bytes(r.session_id)?,
                },
                Msg::ProgramStatus(p) => Self::ProgramStatus {
                    session_id: id_from_bytes(p.session_id)?,
                    records: p
                        .records
                        .into_iter()
                        .map(TryInto::try_into)
                        .collect::<Result<_, _>>()?,
                },
                Msg::PromptShown(p) => Self::PromptShown {
                    session_id: id_from_bytes(p.session_id)?,
                },
                Msg::CommandFinished(c) => Self::CommandFinished {
                    session_id: id_from_bytes(c.session_id)?,
                    exit_code: c.exit_code,
                },
                Msg::SessionChanged(c) => Self::SessionChanged {
                    session_id: id_from_bytes(c.session_id)?,
                    fields: c
                        .fields
                        .into_iter()
                        .map(session_field_from_wire)
                        .collect::<Result<_, _>>()?,
                },
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The unset sentinel names no kind: a subscribe carrying it would
    /// read as asking for something the client never chose.
    #[test]
    fn an_unspecified_kind_is_refused() {
        let wire = v1::NotifyToDaemonMsg {
            msg: Some(v1::notify_to_daemon_msg::Msg::Subscribe(
                v1::NotifySubscribe {
                    session_prefix: None,
                    kinds: vec![v1::ObserverKind::Unspecified as i32],
                },
            )),
            correlation: None,
        };
        assert!(matches!(
            messages::NotifyToDaemonMsg::try_from(wire),
            Err(WireError::UnspecifiedEnum("ObserverKind"))
        ));
    }

    #[test]
    fn an_unknown_session_field_is_refused() {
        let wire = v1::NotifyToClientMsg {
            msg: Some(v1::notify_to_client_msg::Msg::SessionChanged(
                v1::NotifySessionChanged {
                    session_id: id_to_bytes(1),
                    fields: vec![99],
                },
            )),
            correlation: None,
        };
        assert!(matches!(
            messages::NotifyToClientMsg::try_from(wire),
            Err(WireError::UnknownEnum { value: 99, .. })
        ));
    }
}

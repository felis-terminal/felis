//! `SessionToDaemonMsg` / `SessionToClientMsg` <-> wire.

use super::{
    WireError, decode_enum, id_from_bytes, id_to_bytes, opt_rgb_from_wire, opt_rgb_to_wire,
};
use crate::messages;
use crate::wire::v1;

const fn attach_failure_to_wire(f: messages::AttachFailure) -> v1::AttachFailure {
    use messages::AttachFailure as F;
    match f {
        F::UnknownSession => v1::AttachFailure::UnknownSession,
        F::SessionEnding => v1::AttachFailure::SessionEnding,
        F::SessionExited => v1::AttachFailure::SessionExited,
        F::NoMatch => v1::AttachFailure::NoMatch,
        F::Ambiguous => v1::AttachFailure::Ambiguous,
    }
}

/// `UNSPECIFIED` is a hard error: a defaulted reason would report a
/// refusal the daemon never gave.
fn attach_failure_from_wire(raw: i32) -> Result<messages::AttachFailure, WireError> {
    use messages::AttachFailure as F;
    match decode_enum::<v1::AttachFailure>("AttachFailure", raw)? {
        v1::AttachFailure::Unspecified => Err(WireError::UnspecifiedEnum("AttachFailure")),
        v1::AttachFailure::UnknownSession => Ok(F::UnknownSession),
        v1::AttachFailure::SessionEnding => Ok(F::SessionEnding),
        v1::AttachFailure::SessionExited => Ok(F::SessionExited),
        v1::AttachFailure::NoMatch => Ok(F::NoMatch),
        v1::AttachFailure::Ambiguous => Ok(F::Ambiguous),
    }
}

pub(super) const fn create_failure_to_wire(f: messages::CreateFailure) -> v1::CreateFailure {
    use messages::CreateFailure as F;
    match f {
        F::SpawnFailed => v1::CreateFailure::SpawnFailed,
        F::GeometryOutOfRange => v1::CreateFailure::GeometryOutOfRange,
        F::SessionLimitReached => v1::CreateFailure::SessionLimitReached,
        F::DaemonDraining => v1::CreateFailure::DaemonDraining,
    }
}

pub(super) fn create_failure_from_wire(raw: i32) -> Result<messages::CreateFailure, WireError> {
    use messages::CreateFailure as F;
    match decode_enum::<v1::CreateFailure>("CreateFailure", raw)? {
        v1::CreateFailure::Unspecified => Err(WireError::UnspecifiedEnum("CreateFailure")),
        v1::CreateFailure::SpawnFailed => Ok(F::SpawnFailed),
        v1::CreateFailure::GeometryOutOfRange => Ok(F::GeometryOutOfRange),
        v1::CreateFailure::SessionLimitReached => Ok(F::SessionLimitReached),
        v1::CreateFailure::DaemonDraining => Ok(F::DaemonDraining),
    }
}

const fn attach_refusal_to_wire(r: messages::AttachRefusal) -> v1::session_attach_failed::Reason {
    use v1::session_attach_failed::Reason;
    match r {
        messages::AttachRefusal::Attach(f) => Reason::Attach(attach_failure_to_wire(f) as i32),
        messages::AttachRefusal::Create(f) => Reason::Create(create_failure_to_wire(f) as i32),
    }
}

fn attach_refusal_from_wire(
    reason: Option<v1::session_attach_failed::Reason>,
) -> Result<messages::AttachRefusal, WireError> {
    use v1::session_attach_failed::Reason;
    match reason.ok_or(WireError::MissingOneof("SessionAttachFailed.reason"))? {
        Reason::Attach(raw) => Ok(messages::AttachRefusal::Attach(attach_failure_from_wire(
            raw,
        )?)),
        Reason::Create(raw) => Ok(messages::AttachRefusal::Create(create_failure_from_wire(
            raw,
        )?)),
    }
}

fn attach_target_from_wire(
    target: Option<v1::session_attach::Target>,
) -> Result<messages::AttachTarget, WireError> {
    use v1::session_attach::Target as W;
    match target.ok_or(WireError::MissingOneof("SessionAttach.target"))? {
        W::Id(id) => Ok(messages::AttachTarget::Id(id_from_bytes(id)?)),
        W::IdPrefix(prefix) => Ok(messages::AttachTarget::Prefix(prefix)),
    }
}

fn env_pair_to_wire((key, value): &(String, String)) -> v1::EnvPair {
    v1::EnvPair {
        key: key.clone(),
        value: value.clone(),
    }
}

fn env_pair_from_wire(p: v1::EnvPair) -> (String, String) {
    (p.key, p.value)
}

impl From<&messages::SpawnArgs> for v1::SpawnArgs {
    fn from(a: &messages::SpawnArgs) -> Self {
        Self {
            command: a.command.clone(),
            args: a.args.clone(),
            cwd: a.cwd.clone(),
            env: a.env.iter().map(env_pair_to_wire).collect(),
            dims: a.dims.map(Into::into),
            tags: a.tags.clone(),
            env_base: a.env_base.as_ref().map(|entries| v1::EnvBase {
                entries: entries
                    .iter()
                    .map(|(key, value)| v1::EnvBytesPair {
                        key: key.clone(),
                        value: value.clone(),
                    })
                    .collect(),
            }),
        }
    }
}

impl TryFrom<v1::SpawnArgs> for messages::SpawnArgs {
    type Error = WireError;
    fn try_from(a: v1::SpawnArgs) -> Result<Self, Self::Error> {
        Ok(Self {
            command: a.command,
            args: a.args,
            cwd: a.cwd,
            env: a.env.into_iter().map(env_pair_from_wire).collect(),
            // Absence is the create asking for the daemon default, so
            // unlike every other GridDims field a missing one is not a
            // truncated peer.
            dims: a.dims.map(Into::into),
            tags: a.tags,
            // The wire's optional message is what says "an empty base",
            // which an empty repeated field could not.
            env_base: a
                .env_base
                .map(|base| base.entries.into_iter().map(|p| (p.key, p.value)).collect()),
        })
    }
}

impl From<&messages::SessionToDaemonMsg> for v1::SessionToDaemonMsg {
    fn from(m: &messages::SessionToDaemonMsg) -> Self {
        use messages::SessionToDaemonMsg as S;
        use v1::session_to_daemon_msg::Msg;
        let msg = match m {
            S::Attach { target, live_only } => Msg::Attach(v1::SessionAttach {
                target: Some(match target {
                    messages::AttachTarget::Id(id) => {
                        v1::session_attach::Target::Id(id_to_bytes(*id))
                    }
                    messages::AttachTarget::Prefix(prefix) => {
                        v1::session_attach::Target::IdPrefix(prefix.clone())
                    }
                }),
                live_only: *live_only,
            }),
            S::Create { args } => Msg::Create(v1::SessionCreate {
                args: Some(args.into()),
            }),
            S::Detach => Msg::Detach(v1::SessionDetach {}),
            S::ConfigureTheme { fg, bg, cursor } => {
                Msg::ConfigureTheme(v1::SessionConfigureTheme {
                    fg: opt_rgb_to_wire(*fg),
                    bg: opt_rgb_to_wire(*bg),
                    cursor: opt_rgb_to_wire(*cursor),
                })
            }
            S::InputFence => Msg::InputFence(v1::SessionInputFence {}),
        };
        Self {
            msg: Some(msg),
            correlation: None,
        }
    }
}

impl From<&messages::SessionToClientMsg> for v1::SessionToClientMsg {
    fn from(m: &messages::SessionToClientMsg) -> Self {
        use messages::SessionToClientMsg as S;
        use v1::session_to_client_msg::Msg;
        let msg = match m {
            S::Attached { info } => Msg::Attached(v1::SessionAttached {
                info: Some(info.into()),
            }),
            S::AttachFailed { reason, detail } => Msg::AttachFailed(v1::SessionAttachFailed {
                reason: Some(attach_refusal_to_wire(*reason)),
                detail: detail.clone(),
            }),
            S::Created { info } => Msg::Created(v1::SessionCreated {
                info: Some(info.into()),
            }),
            S::InputAccepted => Msg::InputAccepted(v1::SessionInputAccepted {}),
        };
        Self {
            msg: Some(msg),
            correlation: None,
        }
    }
}

impl TryFrom<v1::SessionToDaemonMsg> for messages::SessionToDaemonMsg {
    type Error = WireError;
    fn try_from(m: v1::SessionToDaemonMsg) -> Result<Self, Self::Error> {
        use v1::session_to_daemon_msg::Msg;
        Ok(
            match m
                .msg
                .ok_or(WireError::MissingOneof("SessionToDaemonMsg.msg"))?
            {
                Msg::Attach(a) => Self::Attach {
                    target: attach_target_from_wire(a.target)?,
                    live_only: a.live_only,
                },
                Msg::Create(c) => Self::Create {
                    args: messages::SpawnArgs::try_from(
                        c.args.ok_or(WireError::MissingField("Create.args"))?,
                    )?,
                },
                Msg::Detach(_) => Self::Detach,
                Msg::ConfigureTheme(t) => Self::ConfigureTheme {
                    fg: opt_rgb_from_wire(t.fg)?,
                    bg: opt_rgb_from_wire(t.bg)?,
                    cursor: opt_rgb_from_wire(t.cursor)?,
                },
                Msg::InputFence(_) => Self::InputFence,
            },
        )
    }
}

impl TryFrom<v1::SessionToClientMsg> for messages::SessionToClientMsg {
    type Error = WireError;
    fn try_from(m: v1::SessionToClientMsg) -> Result<Self, Self::Error> {
        use v1::session_to_client_msg::Msg;
        Ok(
            match m
                .msg
                .ok_or(WireError::MissingOneof("SessionToClientMsg.msg"))?
            {
                Msg::Attached(a) => Self::Attached {
                    info: a
                        .info
                        .ok_or(WireError::MissingField("SessionAttached.info"))?
                        .try_into()?,
                },
                Msg::AttachFailed(a) => Self::AttachFailed {
                    reason: attach_refusal_from_wire(a.reason)?,
                    detail: a.detail,
                },
                Msg::Created(c) => Self::Created {
                    info: c
                        .info
                        .ok_or(WireError::MissingField("SessionCreated.info"))?
                        .try_into()?,
                },
                Msg::InputAccepted(_) => Self::InputAccepted,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire_attach(target: Option<v1::session_attach::Target>) -> v1::SessionToDaemonMsg {
        v1::SessionToDaemonMsg {
            msg: Some(v1::session_to_daemon_msg::Msg::Attach(v1::SessionAttach {
                target,
                live_only: false,
            })),
            correlation: None,
        }
    }

    #[test]
    fn an_attach_naming_no_target_at_all_is_rejected() {
        let err = messages::SessionToDaemonMsg::try_from(wire_attach(None))
            .expect_err("an attach must name a session");
        assert!(
            matches!(err, WireError::MissingOneof("SessionAttach.target")),
            "{err:?}"
        );
    }

    /// A sibling-field pair would admit a frame naming two sessions;
    /// the union's single slot does not.
    #[test]
    fn a_second_target_replaces_the_first_instead_of_naming_two() {
        let mut attach = v1::SessionAttach {
            target: Some(v1::session_attach::Target::Id(id_to_bytes(0xCAFE))),
            live_only: false,
        };
        attach.target = Some(v1::session_attach::Target::IdPrefix("beef".into()));
        assert_eq!(
            messages::SessionToDaemonMsg::try_from(wire_attach(attach.target)).unwrap(),
            messages::SessionToDaemonMsg::Attach {
                target: messages::AttachTarget::Prefix("beef".into()),
                live_only: false,
            }
        );
    }

    fn wire_attach_failed(
        reason: Option<v1::session_attach_failed::Reason>,
    ) -> v1::SessionToClientMsg {
        v1::SessionToClientMsg {
            msg: Some(v1::session_to_client_msg::Msg::AttachFailed(
                v1::SessionAttachFailed {
                    reason,
                    detail: String::new(),
                },
            )),
            correlation: None,
        }
    }

    #[test]
    fn a_refusal_naming_no_half_at_all_is_rejected() {
        let err = messages::SessionToClientMsg::try_from(wire_attach_failed(None))
            .expect_err("a refusal must name which half refused");
        assert!(
            matches!(err, WireError::MissingOneof("SessionAttachFailed.reason")),
            "{err:?}"
        );
    }

    #[test]
    fn an_unspecified_reason_is_rejected_on_either_half() {
        use v1::session_attach_failed::Reason;

        for (arm, named) in [
            (Reason::Attach(0), "AttachFailure"),
            (Reason::Create(0), "CreateFailure"),
        ] {
            let err = messages::SessionToClientMsg::try_from(wire_attach_failed(Some(arm)))
                .expect_err("UNSPECIFIED is no refusal the daemon gave");
            assert!(
                matches!(err, WireError::UnspecifiedEnum(e) if e == named),
                "{err:?}"
            );
        }
    }

    #[test]
    fn an_attach_naming_exactly_one_target_decodes_to_that_arm() {
        assert_eq!(
            messages::SessionToDaemonMsg::try_from(wire_attach(Some(
                v1::session_attach::Target::Id(id_to_bytes(0xCAFE))
            )))
            .unwrap(),
            messages::SessionToDaemonMsg::Attach {
                target: messages::AttachTarget::Id(0xCAFE),
                live_only: false,
            }
        );
        assert_eq!(
            messages::SessionToDaemonMsg::try_from(wire_attach(Some(
                v1::session_attach::Target::IdPrefix("cafe".into())
            )))
            .unwrap(),
            messages::SessionToDaemonMsg::Attach {
                target: messages::AttachTarget::Prefix("cafe".into()),
                live_only: false,
            }
        );
    }
}

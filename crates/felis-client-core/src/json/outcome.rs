//! The daemon's answers as protocol results: a resolved session id or
//! the error kind that stands for its failure.

use felis_protocol::messages::{
    ExitStatus, InfoOutcome, PushMsg, ResolvedId, SendOutcome, SessionInfo,
};

use super::common::SessionId;
use super::protocol::{ErrorKind, ErrorObject, SessionRow};
use super::session::SessionObject;

impl ErrorObject {
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            session: None,
        }
    }

    /// The terminal a push ends an attach stream with. A switch names
    /// its target session; a retarget names none, since the daemon it
    /// points at is not this one.
    #[must_use]
    pub fn from_push(push: &PushMsg) -> Self {
        match push {
            PushMsg::Evicted { reason } => Self::new(ErrorKind::Evicted, reason.clone()),
            PushMsg::SessionExited { status, .. } => Self::new(
                ErrorKind::SessionEnded,
                match status {
                    Some(ExitStatus::Code(code)) => format!("the program exited with {code}"),
                    Some(ExitStatus::Signal(signal)) => {
                        format!("the program was killed by signal {signal}")
                    }
                    None => "the program exited".to_owned(),
                },
            ),
            PushMsg::Reattach { id } => Self {
                session: Some(SessionId(*id)),
                ..Self::new(
                    ErrorKind::Moved,
                    "another client switched this window to another session",
                )
            },
            PushMsg::RetargetHost { .. } => Self::new(
                ErrorKind::Moved,
                "another client switched this window to another daemon",
            ),
        }
    }
}

fn no_match() -> ErrorObject {
    ErrorObject::new(ErrorKind::NoMatch, "no session has this id")
}

fn ambiguous(matches: u32) -> ErrorObject {
    ErrorObject::new(
        ErrorKind::Ambiguous,
        format!("{matches} sessions match this id"),
    )
}

impl SessionRow {
    #[must_use]
    pub fn from_info(info: &SessionInfo, short_id: String) -> Self {
        Self {
            session: SessionObject::from_info(info, short_id),
            sequence: info.sequence.get(),
        }
    }

    /// Shortened against `roster`, as a listing is.
    #[must_use]
    pub fn in_roster(info: &SessionInfo, roster: &[SessionInfo]) -> Self {
        Self {
            session: SessionObject::in_roster(info, roster),
            sequence: info.sequence.get(),
        }
    }
}

impl TryFrom<ResolvedId> for SessionId {
    type Error = ErrorObject;

    fn try_from(resolved: ResolvedId) -> Result<Self, ErrorObject> {
        match resolved {
            ResolvedId::Ok { id } => Ok(Self(id)),
            ResolvedId::NoMatch => Err(no_match()),
            ResolvedId::Ambiguous { matches } => Err(ambiguous(matches)),
        }
    }
}

impl TryFrom<InfoOutcome> for SessionRow {
    type Error = ErrorObject;

    fn try_from(outcome: InfoOutcome) -> Result<Self, ErrorObject> {
        match outcome {
            InfoOutcome::Found { session, short_id } => Ok(Self::from_info(&session, short_id)),
            InfoOutcome::NoMatch => Err(no_match()),
            InfoOutcome::Ambiguous { matches } => Err(ambiguous(matches)),
        }
    }
}

impl TryFrom<SendOutcome> for SessionId {
    type Error = ErrorObject;

    fn try_from(outcome: SendOutcome) -> Result<Self, ErrorObject> {
        match outcome {
            SendOutcome::Sent { id } => Ok(Self(id)),
            SendOutcome::NoMatch => Err(no_match()),
            SendOutcome::Ambiguous { matches } => Err(ambiguous(matches)),
            SendOutcome::Refused {
                reason,
                step,
                detail,
            } => Err(ErrorObject::new(
                reason.into(),
                match step {
                    Some(step) => format!("step {step}: {detail}"),
                    None => detail,
                },
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use felis_protocol::messages::SendRefusal;

    use super::*;

    #[test]
    fn an_unresolved_id_names_which_way_it_failed() {
        assert_eq!(
            SessionId::try_from(ResolvedId::NoMatch).unwrap_err().kind,
            ErrorKind::NoMatch
        );
        assert_eq!(
            SessionId::try_from(ResolvedId::Ambiguous { matches: 2 })
                .unwrap_err()
                .kind,
            ErrorKind::Ambiguous
        );
        assert_eq!(
            SessionId::try_from(ResolvedId::Ok { id: 7 }).unwrap(),
            SessionId(7)
        );
    }

    #[test]
    fn a_send_refused_for_its_session_ending_is_session_ended() {
        let refused = SendOutcome::Refused {
            reason: SendRefusal::SessionEnded,
            step: None,
            detail: String::new(),
        };
        assert_eq!(
            SessionId::try_from(refused).unwrap_err().kind,
            ErrorKind::SessionEnded
        );
        let invalid = SendOutcome::Refused {
            reason: SendRefusal::Unreported,
            step: Some(1),
            detail: "mouse reporting is off".to_owned(),
        };
        let error = SessionId::try_from(invalid).unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        assert_eq!(error.message, "step 1: mouse reporting is off");
    }

    #[test]
    fn every_push_ends_an_attach_and_only_a_switch_names_a_session() {
        let ends = |push: PushMsg| {
            let error = ErrorObject::from_push(&push);
            (error.kind, error.session)
        };
        assert_eq!(
            ends(PushMsg::Evicted {
                reason: String::new()
            }),
            (ErrorKind::Evicted, None)
        );
        assert_eq!(
            ends(PushMsg::SessionExited {
                id: 1,
                status: Some(ExitStatus::Code(3)),
            }),
            (ErrorKind::SessionEnded, None)
        );
        assert_eq!(
            ends(PushMsg::Reattach { id: 2 }),
            (ErrorKind::Moved, Some(SessionId(2)))
        );
    }
}

//! [`NotifyToDaemonMsg`] and [`NotifyToClientMsg`]: notification observer
//! connections (kind 7).

use serde::{Deserialize, Serialize};

use super::{ArmMeta, Directed};

use super::{ExitStatus, Notification, ProgramStatusRecord};

/// What an observer stream relays
/// ([`NotifyToDaemonMsg::Subscribe::kinds`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ObserverKind {
    /// [`NotifyToClientMsg::Event`].
    Notification,
    /// [`NotifyToClientMsg::SessionAdded`], [`NotifyToClientMsg::SessionExited`]
    /// and [`NotifyToClientMsg::SessionRemoved`].
    Lifecycle,
    /// [`NotifyToClientMsg::ProgramStatus`].
    ProgramStatus,
    /// [`NotifyToClientMsg::PromptShown`] and [`NotifyToClientMsg::CommandFinished`].
    Prompt,
    /// [`NotifyToClientMsg::SessionChanged`].
    Meta,
}

impl ObserverKind {
    pub const ALL: [Self; 5] = [
        Self::Notification,
        Self::Lifecycle,
        Self::ProgramStatus,
        Self::Prompt,
        Self::Meta,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Notification => "notification",
            Self::Lifecycle => "lifecycle",
            Self::ProgramStatus => "program_status",
            Self::Prompt => "prompt",
            Self::Meta => "meta",
        }
    }

    /// The kind an item belongs to; `None` for the stream's own
    /// `Subscribed` and `Lagged`.
    #[must_use]
    pub const fn of(msg: &NotifyToClientMsg) -> Option<Self> {
        match msg {
            NotifyToClientMsg::Event { .. } => Some(Self::Notification),
            NotifyToClientMsg::SessionAdded { .. }
            | NotifyToClientMsg::SessionExited { .. }
            | NotifyToClientMsg::SessionRemoved { .. } => Some(Self::Lifecycle),
            NotifyToClientMsg::ProgramStatus { .. } => Some(Self::ProgramStatus),
            NotifyToClientMsg::PromptShown { .. } | NotifyToClientMsg::CommandFinished { .. } => {
                Some(Self::Prompt)
            }
            NotifyToClientMsg::SessionChanged { .. } => Some(Self::Meta),
            NotifyToClientMsg::Subscribed { .. } | NotifyToClientMsg::Lagged { .. } => None,
        }
    }
}

/// A session row field whose change [`NotifyToClientMsg::SessionChanged`]
/// names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SessionField {
    Tags,
    Title,
    Cwd,
    Attachments,
}

impl SessionField {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tags => "tags",
            Self::Title => "title",
            Self::Cwd => "cwd",
            Self::Attachments => "attachments",
        }
    }
}

/// Notification observer requests ([`crate::MessageKind::Notify`],
/// kind 7): the client's half of the [`crate::ConnectionMode::Observer`]
/// mode's whole surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotifyToDaemonMsg {
    /// In place of an attach: receive a
    /// [`NotifyToClientMsg::Event`] for every desktop notification any session fires
    /// (`docs/reference/protocols/notifications.md`). The daemon replies
    /// [`NotifyToClientMsg::Subscribed`] and then streams events until it closes.
    Subscribe {
        /// Only relay items about the session this lowercase hex
        /// prefix resolves to; resolved daemon-side.
        session_prefix: Option<String>,
        /// Empty is notifications alone, the subscribe of a peer that
        /// predates the field.
        kinds: Vec<ObserverKind>,
    },
}

/// The daemon's half of the notification observer family: the items of
/// the stream a [`NotifyToDaemonMsg::Subscribe`] opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotifyToClientMsg {
    /// Daemon → observer. One decoded desktop notification plus the
    /// context the daemon already tracks; the popup is the consumer's
    /// job.
    Event {
        session_id: u128,
        notification: Notification,
        /// Producer's OSC 99 `i=` identifier, if any. Opaque.
        notify_id: Option<String>,
        /// The session's window title (OSC 0/2).
        session_title: Option<String>,
        /// The session's working directory (OSC 7), if known.
        cwd: Option<String>,
        /// Whether a GUI client is live on this session. An attached
        /// session already flashed its own window
        /// ([`GridMsg::Attention`](super::GridMsg::Attention)), so a
        /// consumer can `notify-send` only the detached ones.
        attached: bool,
    },
    /// Daemon → observer. Acknowledges [`NotifyToDaemonMsg::Subscribe`] before the
    /// event stream, so a bad prefix surfaces as a typed reply, never
    /// a bare close.
    Subscribed {
        /// `None` when the subscribe carried no `session_prefix`.
        filter: Option<super::ResolvedId>,
    },
    /// Daemon → observer. `missed` events were dropped while the
    /// observer fell behind the notification ring. Loss under
    /// backpressure is part of the observer contract, but never silent.
    Lagged { missed: u64 },
    /// Daemon → observer. The session entered the listed roster; the
    /// first item about it.
    SessionAdded { session_id: u128 },
    /// Daemon → observer. The session's child ended, once per session.
    /// `None` when no status could be reaped.
    SessionExited {
        session_id: u128,
        status: Option<ExitStatus>,
    },
    /// Daemon → observer. The session left the pool; the last item its
    /// own task publishes about it.
    SessionRemoved { session_id: u128 },
    /// Daemon → observer. The whole OSC 7501 record set after a change.
    ProgramStatus {
        session_id: u128,
        records: Vec<ProgramStatusRecord>,
    },
    /// Daemon → observer. A primary OSC 133 `A` mark.
    PromptShown { session_id: u128 },
    /// Daemon → observer. An OSC 133 `D` mark; `None` for a bare `D`.
    CommandFinished {
        session_id: u128,
        exit_code: Option<u32>,
    },
    /// Daemon → observer. These row fields changed; their values are
    /// the session row's.
    SessionChanged {
        session_id: u128,
        fields: Vec<SessionField>,
    },
}

impl NotifyToClientMsg {
    /// The session an item is about; `None` for `Subscribed` and
    /// `Lagged`.
    #[must_use]
    pub const fn session_id(&self) -> Option<u128> {
        match self {
            Self::Event { session_id, .. }
            | Self::SessionAdded { session_id }
            | Self::SessionExited { session_id, .. }
            | Self::SessionRemoved { session_id }
            | Self::ProgramStatus { session_id, .. }
            | Self::PromptShown { session_id }
            | Self::CommandFinished { session_id, .. }
            | Self::SessionChanged { session_id, .. } => Some(*session_id),
            Self::Subscribed { .. } | Self::Lagged { .. } => None,
        }
    }
}

impl Directed for NotifyToDaemonMsg {
    const ARMS: &'static [ArmMeta] = crate::schema_meta::NOTIFY_TO_DAEMON_MSG_ARMS;

    fn arm_index(&self) -> usize {
        match self {
            Self::Subscribe { .. } => 0,
        }
    }
}

impl Directed for NotifyToClientMsg {
    const ARMS: &'static [ArmMeta] = crate::schema_meta::NOTIFY_TO_CLIENT_MSG_ARMS;

    fn arm_index(&self) -> usize {
        match self {
            Self::Event { .. } => 0,
            Self::Subscribed { .. } => 1,
            Self::Lagged { .. } => 2,
            Self::SessionAdded { .. } => 3,
            Self::SessionExited { .. } => 4,
            Self::SessionRemoved { .. } => 5,
            Self::ProgramStatus { .. } => 6,
            Self::PromptShown { .. } => 7,
            Self::CommandFinished { .. } => 8,
            Self::SessionChanged { .. } => 9,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::messages::Urgency;
    use crate::messages::test_support::{assert_covers_every_arm, roundtrip};

    fn notify_to_daemon_cases() -> Vec<NotifyToDaemonMsg> {
        vec![
            NotifyToDaemonMsg::Subscribe {
                session_prefix: None,
                kinds: Vec::new(),
            },
            NotifyToDaemonMsg::Subscribe {
                session_prefix: Some("cafe".into()),
                kinds: Vec::new(),
            },
            NotifyToDaemonMsg::Subscribe {
                session_prefix: None,
                kinds: ObserverKind::ALL.to_vec(),
            },
        ]
    }

    fn notify_to_client_cases() -> Vec<NotifyToClientMsg> {
        vec![
            NotifyToClientMsg::Subscribed { filter: None },
            NotifyToClientMsg::Subscribed {
                filter: Some(crate::messages::ResolvedId::Ok {
                    id: 0xCAFE_BABE_DEAD_BEEF,
                }),
            },
            NotifyToClientMsg::Subscribed {
                filter: Some(crate::messages::ResolvedId::NoMatch),
            },
            NotifyToClientMsg::Lagged { missed: 17 },
            NotifyToClientMsg::SessionAdded { session_id: 0x51 },
            NotifyToClientMsg::SessionExited {
                session_id: 0x51,
                status: Some(ExitStatus::Code(3)),
            },
            NotifyToClientMsg::SessionExited {
                session_id: 0x51,
                status: Some(ExitStatus::Signal(9)),
            },
            NotifyToClientMsg::SessionExited {
                session_id: 0x51,
                status: None,
            },
            NotifyToClientMsg::SessionRemoved { session_id: 0x51 },
            NotifyToClientMsg::ProgramStatus {
                session_id: 0x51,
                records: vec![ProgramStatusRecord {
                    id: String::new(),
                    state: "working".into(),
                    kind: Some("agent".into()),
                    progress: Some(40),
                    app: Some("claude".into()),
                    title: None,
                    msg: Some("editing".into()),
                }],
            },
            NotifyToClientMsg::ProgramStatus {
                session_id: 0x51,
                records: Vec::new(),
            },
            NotifyToClientMsg::PromptShown { session_id: 0x51 },
            NotifyToClientMsg::CommandFinished {
                session_id: 0x51,
                exit_code: Some(1),
            },
            NotifyToClientMsg::CommandFinished {
                session_id: 0x51,
                exit_code: None,
            },
            NotifyToClientMsg::SessionChanged {
                session_id: 0x51,
                fields: vec![SessionField::Tags, SessionField::Attachments],
            },
            NotifyToClientMsg::Event {
                session_id: 0xCAFE_BABE_DEAD_BEEF,
                notification: Notification {
                    title: Some("Build finished".into()),
                    body: "0 errors".into(),
                    urgency: Urgency::Critical,
                },
                notify_id: Some("build-42".into()),
                session_title: Some("make".into()),
                cwd: Some("/home/me/src/felis".into()),
                attached: false,
            },
            NotifyToClientMsg::Event {
                session_id: 0x77,
                notification: Notification {
                    title: Some("idle".into()),
                    body: "still running".into(),
                    urgency: Urgency::Low,
                },
                notify_id: None,
                session_title: None,
                cwd: None,
                attached: true,
            },
            NotifyToClientMsg::Event {
                session_id: 0x1234,
                notification: Notification {
                    title: None,
                    body: "ping".into(),
                    urgency: Urgency::Normal,
                },
                notify_id: None,
                session_title: None,
                cwd: None,
                attached: true,
            },
        ]
    }

    #[test]
    fn notify_messages_round_trip() {
        for msg in notify_to_daemon_cases() {
            assert_eq!(roundtrip(&msg), msg);
        }
        for msg in notify_to_client_cases() {
            assert_eq!(roundtrip(&msg), msg);
        }
    }

    #[test]
    fn notify_cases_cover_every_variant() {
        assert_covers_every_arm(&notify_to_daemon_cases());
        assert_covers_every_arm(&notify_to_client_cases());
    }
}

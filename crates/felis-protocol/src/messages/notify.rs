//! [`NotifyToDaemonMsg`] and [`NotifyToClientMsg`]: notification observer
//! connections (kind 7).

use serde::{Deserialize, Serialize};

use super::{ArmMeta, CorrelationClass, Directed, Direction, ModeSet, PhaseSet};

use super::Notification;

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
        /// Only relay notifications from the session this lowercase
        /// hex prefix resolves to; resolved daemon-side.
        session_prefix: Option<String>,
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
}

const fn arm(name: &'static str, direction: Direction, correlation: CorrelationClass) -> ArmMeta {
    ArmMeta::new(name, direction, correlation, ModeSet::OBSERVER).phases(PhaseSet::OBSERVING)
}

impl Directed for NotifyToDaemonMsg {
    const ARMS: &'static [ArmMeta] = &[
        // The subscribe is what turns a `Setup` connection into an
        // observer, so it alone is legal before the role exists; every
        // stream item belongs to the role it created.
        arm(
            "Notify::Subscribe",
            Direction::ToDaemon,
            CorrelationClass::StreamOpener,
        )
        .phases(PhaseSet::SETUP),
    ];

    fn arm_index(&self) -> usize {
        match self {
            Self::Subscribe { .. } => 0,
        }
    }
}

impl Directed for NotifyToClientMsg {
    const ARMS: &'static [ArmMeta] = &[
        arm(
            "Notify::Event",
            Direction::ToClient,
            CorrelationClass::StreamItem,
        ),
        arm(
            "Notify::Subscribed",
            Direction::ToClient,
            CorrelationClass::StreamItem,
        ),
        arm(
            "Notify::Lagged",
            Direction::ToClient,
            CorrelationClass::StreamItem,
        ),
    ];

    fn arm_index(&self) -> usize {
        match self {
            Self::Event { .. } => 0,
            Self::Subscribed { .. } => 1,
            Self::Lagged { .. } => 2,
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
            },
            NotifyToDaemonMsg::Subscribe {
                session_prefix: Some("cafe".into()),
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

//! [`ConnToDaemonMsg`] and [`ConnToClientMsg`]: the connection
//! handshake and the stream lifecycle (kind 0).

use serde::{Deserialize, Serialize};

use crate::build_identity::BuildIdentity;
use crate::caps::ConnectionMode;
use crate::messages::{
    ArmMeta, CorrelationClass, Directed, Direction, ModeSet, PhaseSet, StreamErrorReason, StreamId,
    Subject,
};

/// The client's half of the connection family
/// ([`crate::MessageKind::Conn`], kind 0): the handshake's opening and
/// the stream cancel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnToDaemonMsg {
    /// First message on a new connection. The preface
    /// ([`crate::preface`]) settled the version; `mode` is the one
    /// field the effective minor still gates
    /// (`docs/reference/ipc.md` "Connection modes").
    Hello {
        /// A mode a peer cannot read costs the connection, so a sender
        /// names only what the effective minor defines and a daemon
        /// answers anything else with [`RefusalReason::UnknownMode`].
        mode: ConnectionMode,
        /// Demand-driven grid frame pacing: the client sends
        /// [`InputMsg::NextGridFrame`](super::InputMsg::NextGridFrame)
        /// once per vsync and the daemon emits a coalesced diff only in
        /// response. Everything else the daemon ships unconditionally
        /// (`docs/reference/ipc.md` "Versioning").
        pull_paced: bool,
    },
    /// Stop producing items for this stream. Best-effort: the client
    /// keeps accepting items until the terminal arrives, and a cancel
    /// naming a stream that already terminated is an idempotent no-op.
    /// A cancel naming a stream that was never opened is corruption.
    Cancel { stream_id: StreamId },
}

/// The daemon's half of the connection family: the handshake's answer,
/// its refusals, and the stream terminals. The terminals live here
/// rather than once per streaming family so the shared connection
/// driver can enforce "exactly one terminal per stream" for every
/// family at once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnToClientMsg {
    /// Reply to [`ConnToDaemonMsg::Hello`]. Carries no session roster;
    /// listing is [`OpsToDaemonMsg::List`](super::OpsToDaemonMsg::List)'s
    /// job.
    Welcome {
        /// The daemon's build, typed, so `felis version` compares
        /// identities instead of scraping text. Never a compatibility
        /// signal.
        identity: Option<BuildIdentity>,
    },
    /// Sent in place of [`Self::Welcome`] (or, for a mode violation, in
    /// place of the answer the peer asked for), after which the daemon
    /// closes. Without it a refusal is a bare close the client cannot
    /// tell from a transient EOF.
    Refused {
        reason: RefusalReason,
        /// Human-readable specifics. Never parsed.
        detail: String,
    },
    /// Exactly one [`Self::End`] or [`Self::Error`] per stream; an item
    /// or a second terminal after it is corruption.
    End {
        stream_id: StreamId,
        /// Items that preceded this terminator; `0` is normal.
        count: u32,
    },
    /// A request that will produce no reply, or a stream's one
    /// terminal.
    Error {
        subject: Subject,
        reason: StreamErrorReason,
        /// Human-readable specifics. Never parsed.
        detail: String,
    },
}

/// Wire metadata, the handshake and the stream lifecycle: every arm is
/// a connection-level fact, so none is tied to a mode or correlated.
const fn arm(name: &'static str, direction: Direction, phases: PhaseSet) -> ArmMeta {
    ArmMeta::new(
        name,
        direction,
        CorrelationClass::Uncorrelated,
        ModeSet::EVERY,
    )
    .phases(phases)
}

impl Directed for ConnToDaemonMsg {
    const ARMS: &'static [ArmMeta] = &[
        arm("Conn::Hello", Direction::ToDaemon, PhaseSet::HANDSHAKE),
        // Stream control, not session traffic: a stream is live in
        // whichever phase opened it, and tying these to one role
        // would leave an observer unable to cancel its own stream.
        arm("Conn::Cancel", Direction::ToDaemon, PhaseSet::LIVE),
    ];

    fn arm_index(&self) -> usize {
        match self {
            Self::Hello { .. } => 0,
            Self::Cancel { .. } => 1,
        }
    }
}

impl Directed for ConnToClientMsg {
    const ARMS: &'static [ArmMeta] = &[
        arm("Conn::Welcome", Direction::ToClient, PhaseSet::HANDSHAKE),
        // The mode gate also refuses a frame the peer sent after its
        // `Welcome`, so this refusal is written in any phase.
        arm("Conn::Refused", Direction::ToClient, PhaseSet::ANY),
        arm("Conn::End", Direction::ToClient, PhaseSet::LIVE),
        arm("Conn::Error", Direction::ToClient, PhaseSet::LIVE),
    ];

    fn arm_index(&self) -> usize {
        match self {
            Self::Welcome { .. } => 0,
            Self::Refused { .. } => 1,
            Self::End { .. } => 2,
            Self::Error { .. } => 3,
        }
    }
}

/// Why a daemon refused a connection ([`ConnToClientMsg::Refused`]). Version
/// skew is not here: the preface ([`crate::preface`]) refuses a major
/// before any frame is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefusalReason {
    /// The frame the peer sent belongs to a family its stated
    /// [`ConnectionMode`] does not admit.
    Role,
    /// The daemon is already serving every connection it admits. Unlike
    /// [`Self::Role`] the peer did nothing wrong, so the remedy is to
    /// retry once a connection frees.
    AtCapacity,
    /// [`ConnToDaemonMsg::Hello`] named a [`ConnectionMode`] this build does not
    /// define. Distinct from [`Self::Role`], which answers a peer whose
    /// mode is known and whose frame does not suit it: here the mode
    /// itself is unreadable, so the remedy is a newer daemon.
    UnknownMode,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::messages::test_support::{assert_covers_every_arm, roundtrip};

    fn conn_to_daemon_cases() -> Vec<ConnToDaemonMsg> {
        vec![
            ConnToDaemonMsg::Hello {
                mode: ConnectionMode::Window,
                pull_paced: true,
            },
            ConnToDaemonMsg::Hello {
                mode: ConnectionMode::Observer,
                pull_paced: false,
            },
            ConnToDaemonMsg::Hello {
                mode: ConnectionMode::Ops,
                pull_paced: false,
            },
            ConnToDaemonMsg::Cancel {
                stream_id: stream(7),
            },
        ]
    }

    fn conn_to_client_cases() -> Vec<ConnToClientMsg> {
        vec![
            ConnToClientMsg::Welcome {
                identity: Some(BuildIdentity::from_build_env(
                    "0.1.0",
                    "e3abf80e3abf80e3abf80e3abf80e3abf80e3abf-dirty",
                )),
            },
            // A Welcome whose optional `identity` is absent on the wire.
            ConnToClientMsg::Welcome { identity: None },
            ConnToClientMsg::Refused {
                reason: RefusalReason::Role,
                detail: "Observer may not send Ops".to_owned(),
            },
            ConnToClientMsg::Refused {
                reason: RefusalReason::UnknownMode,
                detail: "Hello named connection mode 7".to_owned(),
            },
            ConnToClientMsg::Refused {
                reason: RefusalReason::AtCapacity,
                detail: "the daemon is at 1024 of 1024 connections".to_owned(),
            },
            ConnToClientMsg::End {
                stream_id: stream(7),
                count: 42,
            },
            ConnToClientMsg::Error {
                subject: Subject::Stream(stream(9)),
                reason: StreamErrorReason::InvalidRequest,
                detail: "regex parse error".to_owned(),
            },
            ConnToClientMsg::Error {
                subject: Subject::Request(
                    crate::messages::RequestId::new(3).expect("nonzero request id"),
                ),
                reason: StreamErrorReason::TooManyStreams,
                detail: String::new(),
            },
            ConnToClientMsg::Error {
                subject: Subject::Stream(stream(1)),
                reason: StreamErrorReason::Unavailable,
                detail: "session ended".to_owned(),
            },
            ConnToClientMsg::Error {
                subject: Subject::Stream(stream(1)),
                reason: StreamErrorReason::Internal,
                detail: "row encode failed".to_owned(),
            },
        ]
    }

    fn stream(raw: u64) -> StreamId {
        StreamId::new(raw).expect("nonzero stream id")
    }

    #[test]
    fn conn_messages_round_trip() {
        for msg in conn_to_daemon_cases() {
            assert_eq!(roundtrip(&msg), msg);
        }
        for msg in conn_to_client_cases() {
            assert_eq!(roundtrip(&msg), msg);
        }
    }

    #[test]
    fn conn_cases_cover_every_variant() {
        assert_covers_every_arm(&conn_to_daemon_cases());
        assert_covers_every_arm(&conn_to_client_cases());
    }
}

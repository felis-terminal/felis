//! [`PushMsg`]: daemon→client pushes (kind 8).

use serde::{Deserialize, Serialize};

use super::{ArmMeta, CorrelationClass, Directed, Direction, ModeSet};

use super::RetargetTarget;
#[cfg(test)]
use super::{RetargetCarrier, RetargetLanding, SpawnArgs};

/// Daemon→client pushes ([`crate::MessageKind::Push`], kind 8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PushMsg {
    /// Daemon → client. The session this client was streaming has been
    /// taken away by `felis sessions evict`
    /// (`docs/explanation/architecture/control-surfaces.md`); the
    /// session itself survives in the pool.
    Evicted {
        /// Free-text reason for logs / display. Not stable for parsing.
        reason: String,
    },
    /// Daemon → client, [`crate::ConnectionMode::Window`] only. Another
    /// process asked via [`OpsToDaemonMsg::Switch`](super::OpsToDaemonMsg::Switch) that
    /// this client re-attach to the named session; the client treats it
    /// exactly like its own `switch_session` chord.
    Reattach { id: u128 },
    /// Daemon to client cleanly exited shell signal ([`crate::ConnectionMode::Window`] only).
    ///
    /// Distinguishes normal shell exit (PTY EOF) from daemon crashes before
    /// dropping the subscription.
    SessionExited { id: u128 },
    /// Daemon → client, [`crate::ConnectionMode::Window`] only. Another
    /// process asked via [`OpsToDaemonMsg::Switch`](super::OpsToDaemonMsg::Switch) on a
    /// [`SwitchTarget::Carrier`](super::SwitchTarget::Carrier) that this
    /// client re-dial to a different daemon: detach, dial
    /// `target.carrier`, then do what `target.landing` says there.
    RetargetHost {
        /// Relayed verbatim from the CLI request; the daemon never
        /// inspects it.
        target: RetargetTarget,
    },
}

const fn arm(name: &'static str, modes: ModeSet) -> ArmMeta {
    ArmMeta::new(
        name,
        Direction::ToClient,
        CorrelationClass::Uncorrelated,
        modes,
    )
}

impl Directed for PushMsg {
    // Eviction concerns every subscriber; the window-management pushes
    // ask a window to move, which a scripted `Ops` attach has none to do.
    const ARMS: &'static [ArmMeta] = &[
        arm("Push::Evicted", ModeSet::ATTACHERS),
        arm("Push::Reattach", ModeSet::WINDOW),
        arm("Push::SessionExited", ModeSet::WINDOW),
        arm("Push::RetargetHost", ModeSet::WINDOW),
    ];

    fn arm_index(&self) -> usize {
        match self {
            Self::Evicted { .. } => 0,
            Self::Reattach { .. } => 1,
            Self::SessionExited { .. } => 2,
            Self::RetargetHost { .. } => 3,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::messages::test_support::{assert_covers_every_arm, roundtrip};

    fn push_cases() -> Vec<PushMsg> {
        vec![
            PushMsg::Evicted {
                reason: "evicted by `felis sessions evict`".into(),
            },
            PushMsg::Reattach { id: 0xBEEF },
            PushMsg::SessionExited {
                id: 0xCAFE_BABE_DEAD_BEEF,
            },
            PushMsg::RetargetHost {
                target: RetargetTarget {
                    carrier: RetargetCarrier::Ssh {
                        destination: "user@devbox".into(),
                        ssh_args: vec!["-i".into(), "~/.ssh/vm_key".into()],
                    },
                    landing: RetargetLanding::Create(SpawnArgs::default()),
                },
            },
            PushMsg::RetargetHost {
                target: RetargetTarget {
                    carrier: RetargetCarrier::LocalEndpoint("/run/user/1000/felis/alt.sock".into()),
                    landing: RetargetLanding::Attach("ab12".into()),
                },
            },
            PushMsg::RetargetHost {
                target: RetargetTarget {
                    carrier: RetargetCarrier::DefaultLocal,
                    landing: RetargetLanding::Create(SpawnArgs {
                        command: "fish".into(),
                        args: vec!["-l".into()],
                        ..SpawnArgs::default()
                    }),
                },
            },
        ]
    }

    #[test]
    fn push_messages_round_trip() {
        for msg in push_cases() {
            assert_eq!(roundtrip(&msg), msg);
        }
    }

    #[test]
    fn push_cases_cover_every_variant() {
        assert_covers_every_arm(&push_cases());
    }
}

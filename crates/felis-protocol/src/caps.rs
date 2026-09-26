//! Handshake declarations: the client's [`ConnectionMode`].
//!
//! A mode configures routing and output shaping (`docs/reference/ipc.md`
//! "Connection modes"). See `docs/explanation/architecture/ipc.md`.

use serde::{Deserialize, Serialize};

use crate::messages::ModeSet;

/// What a connection is for, stated once in `Hello`. Each mode admits
/// the arms its work needs and nothing else
/// ([`crate::messages::ArmMeta::modes`]); the modes are not a ladder
/// (`docs/reference/ipc.md` "Connection modes").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConnectionMode {
    /// An interactive window: `Session`, `Input`, `Search`, `Region`,
    /// the pure `Ops` queries (`List`, `Info`, `Status`), and, alone among the
    /// modes, the window-management pushes (`Reattach`,
    /// `SessionExited`, `RetargetHost`, the `Attention` flash).
    Window,
    /// A scripted operator (`felis sessions` verbs): the whole `Ops`
    /// family plus attach. No window-management pushes and no grid
    /// stream; `PushMsg::Evicted` still reaches it.
    Ops,
    /// A notification observer (`felis notifications subscribe`):
    /// `Notify` and nothing else; its `--session` filter resolves
    /// daemon-side (`NotifyToDaemonMsg::Subscribe.session_prefix`).
    Observer,
}

impl ConnectionMode {
    /// Whether this mode may attach to a session and speak the
    /// attach-scoped kinds (`Session`, `Input`, `Search`, `Region`).
    #[must_use]
    pub const fn may_attach(self) -> bool {
        ModeSet::ATTACHERS.contains(self)
    }

    /// Whether this mode may issue the mutating `Ops` one-shots
    /// (`Destroy`, `ForceDetach`, `Switch`, `Tag`). The pure queries
    /// (`Ops::List`, `Ops::Info`, `Ops::Status`) are open to every
    /// attach-capable mode.
    #[must_use]
    pub const fn may_operate(self) -> bool {
        ModeSet::OPS.contains(self)
    }

    /// Whether this mode may subscribe to the notification fan-out.
    #[must_use]
    pub const fn may_observe(self) -> bool {
        ModeSet::OBSERVER.contains(self)
    }

    /// Whether the daemon may push window-management frames
    /// (`PushMsg::Reattach` / `SessionExited` / `RetargetHost`) and the
    /// notification `Attention` flash to a subscriber in this mode.
    #[must_use]
    pub const fn is_window(self) -> bool {
        ModeSet::WINDOW.contains(self)
    }

    /// Whether a connection in this mode is exempt from the first-operation
    /// deadline (`docs/reference/ipc.md` "Handshake").
    ///
    /// Long-running operator connections like bridges dial at startup
    /// and sit idle until requested.
    #[must_use]
    pub const fn idles_before_first_operation(self) -> bool {
        matches!(self, Self::Ops)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_mode_admits_only_the_surfaces_its_work_needs() {
        // (mode, may_attach, may_operate, may_observe, is_window, idles)
        let table = [
            (ConnectionMode::Window, true, false, false, true, false),
            (ConnectionMode::Ops, true, true, false, false, true),
            (ConnectionMode::Observer, false, false, true, false, false),
        ];
        for (mode, attach, operate, observe, window, idles) in table {
            assert_eq!(mode.may_attach(), attach, "{mode:?}.may_attach");
            assert_eq!(mode.may_operate(), operate, "{mode:?}.may_operate");
            assert_eq!(mode.may_observe(), observe, "{mode:?}.may_observe");
            assert_eq!(mode.is_window(), window, "{mode:?}.is_window");
            assert_eq!(
                mode.idles_before_first_operation(),
                idles,
                "{mode:?}.idles_before_first_operation"
            );
        }
    }
}

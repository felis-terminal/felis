//! The arm-routing table enforced by the connection driver.
//!
//! Arms define direction, modes, phases, and correlation classes.
//! See `docs/explanation/architecture/ipc.md` "Kind or arm?".

use serde::{Deserialize, Serialize};

use crate::caps::ConnectionMode;

/// Which way a message may travel. Every arm is one-directional, and
/// the connection driver rejects a message that arrived the wrong way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    /// Client → daemon.
    ToDaemon,
    /// Daemon → client.
    ToClient,
}

impl Direction {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ToDaemon => "client→daemon",
            Self::ToClient => "daemon→client",
        }
    }

    /// The token the schema's arm option spells this with, minus its
    /// `DIRECTION_` enum prefix.
    #[must_use]
    pub const fn as_token(self) -> &'static str {
        match self {
            Self::ToDaemon => "to_daemon",
            Self::ToClient => "to_client",
        }
    }
}

/// Connection lifecycle phase and conversational role.
///
/// Disjoint post-handshake conversations are modeled as distinct phases
/// (`docs/explanation/architecture/ipc.md` "The phase ladder").
/// [`Self::Preface`] admits no frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    /// Before the version preface settled: bytes, not frames.
    Preface,
    /// The application handshake (`Conn::Hello` / `Conn::Welcome`).
    Handshake,
    /// Welcomed, but the connection has not yet said what it is for:
    /// it may attach, create, run an `Ops` verb, or subscribe.
    Setup,
    /// Subscribed to a session: the terminal surfaces are live.
    Attached,
    /// Serving a notification stream; no session surface is reachable.
    Observing,
}

/// The phases an arm may arrive in. [`Phase::Preface`] is never a
/// member: the preface is bytes, not frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhaseSet(u8);

impl PhaseSet {
    const HANDSHAKE_BIT: u8 = 1 << 0;
    const SETUP_BIT: u8 = 1 << 1;
    const ATTACHED_BIT: u8 = 1 << 2;
    const OBSERVING_BIT: u8 = 1 << 3;

    /// No phase: the identity the arm-table fold starts from.
    pub(crate) const NONE: Self = Self(0);
    /// The application handshake (`Conn::Hello` / `Conn::Welcome`).
    pub const HANDSHAKE: Self = Self(Self::HANDSHAKE_BIT);
    /// Post-`Welcome`, pre-role: the openers and the `Ops` verbs.
    pub const SETUP: Self = Self(Self::SETUP_BIT);
    /// An attached connection's session surfaces.
    pub const ATTACHED: Self = Self(Self::ATTACHED_BIT);
    /// A notification observer's stream.
    pub const OBSERVING: Self = Self(Self::OBSERVING_BIT);
    /// The `Ops` verbs, which a connection may run before it attaches
    /// and after (a window re-lists without a second connection).
    pub const SETUP_OR_ATTACHED: Self = Self(Self::SETUP_BIT | Self::ATTACHED_BIT);
    /// Every phase in which a stream can be live, so the stream control
    /// arms are not tied to the role that opened one.
    pub const LIVE: Self = Self(Self::SETUP_BIT | Self::ATTACHED_BIT | Self::OBSERVING_BIT);
    /// Every phase that carries frames at all: an arm the daemon may
    /// write whenever it refuses one.
    pub const ANY: Self = Self(Self::HANDSHAKE_BIT | Self::LIVE.0);

    #[must_use]
    pub const fn contains(self, phase: Phase) -> bool {
        let bit = match phase {
            // No frame is admitted before the preface completes, so no
            // arm names this phase.
            Phase::Preface => return false,
            Phase::Handshake => Self::HANDSHAKE_BIT,
            Phase::Setup => Self::SETUP_BIT,
            Phase::Attached => Self::ATTACHED_BIT,
            Phase::Observing => Self::OBSERVING_BIT,
        };
        self.0 & bit != 0
    }

    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The tokens the schema's arm option spells this with, minus the
    /// `PHASE_` enum prefix, in ladder order so the schema and this
    /// table compare as strings. Also the driver's phase diagnostics.
    #[must_use]
    pub fn tokens(self) -> Vec<&'static str> {
        let mut out = Vec::new();
        for (phase, token) in
            Phase::EVERY_FRAMED
                .iter()
                .zip(["handshake", "setup", "attached", "observing"])
        {
            if self.contains(*phase) {
                out.push(token);
            }
        }
        out
    }
}

impl Phase {
    /// Every phase that carries frames, in ladder order. The driver's
    /// table-driven tests enumerate it, and [`PhaseSet::tokens`] spells
    /// it, so a phase added here cannot be forgotten by either.
    pub const EVERY_FRAMED: [Self; 4] = [
        Self::Handshake,
        Self::Setup,
        Self::Attached,
        Self::Observing,
    ];

    /// Every phase, the preface included.
    pub const EVERY: [Self; 5] = [
        Self::Preface,
        Self::Handshake,
        Self::Setup,
        Self::Attached,
        Self::Observing,
    ];
}

/// The connection modes an arm is legal on, receive side. A mode is
/// routing, never authorization (`crate::caps`): the set says which
/// conversations a connection takes part in, not what it may ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeSet(u8);

impl ModeSet {
    const WINDOW_BIT: u8 = 1 << 0;
    const OPS_BIT: u8 = 1 << 1;
    const OBSERVER_BIT: u8 = 1 << 2;

    /// No mode: the identity the arm-table fold starts from.
    pub(crate) const NONE: Self = Self(0);
    pub const WINDOW: Self = Self(Self::WINDOW_BIT);
    pub const OPS: Self = Self(Self::OPS_BIT);
    pub const OBSERVER: Self = Self(Self::OBSERVER_BIT);
    /// The attach-capable modes ([`ConnectionMode::may_attach`]).
    pub const ATTACHERS: Self = Self(Self::WINDOW_BIT | Self::OPS_BIT);
    /// Every mode: the connection-control arms precede any mode-specific
    /// work.
    pub const EVERY: Self = Self(Self::WINDOW_BIT | Self::OPS_BIT | Self::OBSERVER_BIT);

    #[must_use]
    pub const fn contains(self, mode: ConnectionMode) -> bool {
        let bit = match mode {
            ConnectionMode::Window => Self::WINDOW_BIT,
            ConnectionMode::Ops => Self::OPS_BIT,
            ConnectionMode::Observer => Self::OBSERVER_BIT,
        };
        self.0 & bit != 0
    }

    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The tokens the schema's arm option spells this with, minus the
    /// `MODE_` enum prefix, in a fixed order so the schema and this
    /// table compare as strings.
    #[must_use]
    pub fn tokens(self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.contains(ConnectionMode::Window) {
            out.push("window");
        }
        if self.contains(ConnectionMode::Ops) {
            out.push("ops");
        }
        if self.contains(ConnectionMode::Observer) {
            out.push("observer");
        }
        out
    }
}

/// What an arm's [`Correlation`](super::Correlation) envelope means:
/// which half of a request/reply pair or a stream this arm is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorrelationClass {
    /// A connection-scoped push: nothing to match, nothing to close.
    Uncorrelated,
    /// A request that will be answered by exactly one reply arm.
    RequestOpener,
    /// The one reply to a [`Self::RequestOpener`], echoing its
    /// `request_id`.
    RequestReply,
    /// A request that allocates a `stream_id` and is answered by a run
    /// of [`Self::StreamItem`]s closed by
    /// [`ConnToClientMsg::End`](super::ConnToClientMsg::End).
    StreamOpener,
    /// One item of a stream the receiver already serves.
    StreamItem,
}

impl CorrelationClass {
    /// Whether the `stream_id` in this arm's envelope is an id the
    /// sender is allocating now rather than one the receiver already
    /// serves. Only the client allocates ids.
    #[must_use]
    pub const fn opens_stream(self) -> bool {
        matches!(self, Self::StreamOpener)
    }

    /// The envelope this class requires, phrased for the driver's
    /// correlation error. The one place the rule is written, so the
    /// message a peer reads and the check that produced it cannot say
    /// different things.
    #[must_use]
    pub const fn expects(self) -> &'static str {
        match self {
            Self::Uncorrelated => "no correlation envelope",
            Self::RequestOpener => "a request_id equal to the next unissued request",
            Self::RequestReply => "a request_id echoing an outstanding request",
            Self::StreamOpener => "a stream_id allocating the next unopened stream",
            Self::StreamItem => "a stream_id of a live stream",
        }
    }

    /// The token the schema's arm option spells this with, minus its
    /// `CORRELATION_` enum prefix.
    #[must_use]
    pub const fn as_token(self) -> &'static str {
        match self {
            Self::Uncorrelated => "uncorrelated",
            Self::RequestOpener => "request_opener",
            Self::RequestReply => "request_reply",
            Self::StreamOpener => "stream_opener",
            Self::StreamItem => "stream_item",
        }
    }
}

/// One row of the arm-routing table. `felis.proto` restates it in the
/// arm's `(felis.v1.arm)` field option, so a non-Rust peer reads the
/// same matrix out of the descriptor
/// (`the_schema_declares_the_same_arm_table`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArmMeta {
    /// `Family::Variant`, for diagnostics and for the schema comparison.
    pub name: &'static str,
    pub direction: Direction,
    pub correlation: CorrelationClass,
    pub modes: ModeSet,
    pub phases: PhaseSet,
    /// The protocol minor that introduced the arm; `0` for the ones the
    /// major's base schema shipped with (`docs/reference/ipc.md` "The
    /// minor ledger").
    pub since_minor: u16,
}

impl ArmMeta {
    /// The common shape: an attached-connection arm the base schema
    /// shipped.
    #[must_use]
    pub const fn new(
        name: &'static str,
        direction: Direction,
        correlation: CorrelationClass,
        modes: ModeSet,
    ) -> Self {
        Self {
            name,
            direction,
            correlation,
            modes,
            phases: PhaseSet::ATTACHED,
            since_minor: 0,
        }
    }

    #[must_use]
    pub const fn phases(mut self, phases: PhaseSet) -> Self {
        self.phases = phases;
        self
    }

    #[must_use]
    pub const fn since(mut self, minor: u16) -> Self {
        self.since_minor = minor;
        self
    }

    /// Whether two arms declare the same routing columns. The name and
    /// the minor are not routing, so they do not take part.
    #[must_use]
    pub(crate) const fn routes_like(&self, other: &Self) -> bool {
        self.direction as u8 == other.direction as u8
            && self.correlation as u8 == other.correlation as u8
            && self.modes.0 == other.modes.0
            && self.phases.0 == other.phases.0
    }

    /// The body of the `(felis.v1.arm)` option `felis.proto` carries
    /// for this arm: its text-format entries in schema order, joined
    /// by spaces, which is what
    /// `the_schema_declares_the_same_arm_table` normalizes the
    /// schema's multi-line block down to.
    #[must_use]
    pub fn declaration(&self) -> String {
        let mut entries = vec![
            format!(
                "direction: DIRECTION_{}",
                self.direction.as_token().to_ascii_uppercase()
            ),
            format!(
                "correlation: CORRELATION_{}",
                self.correlation.as_token().to_ascii_uppercase()
            ),
        ];
        entries.extend(
            self.modes
                .tokens()
                .iter()
                .map(|mode| format!("modes: MODE_{}", mode.to_ascii_uppercase())),
        );
        entries.extend(
            self.phases
                .tokens()
                .iter()
                .map(|phase| format!("phases: PHASE_{}", phase.to_ascii_uppercase())),
        );
        // Minor 0 is the proto3 default and never reaches the
        // descriptor, so spelling it would compare against a row the
        // schema cannot be written to carry.
        if self.since_minor != 0 {
            entries.push(format!("since: {}", self.since_minor));
        }
        entries.join(" ")
    }
}

/// The modes any arm of a family is legal on.
pub(crate) const fn fold_modes(arms: &[ArmMeta]) -> ModeSet {
    let mut folded = ModeSet::NONE;
    let mut i = 0;
    while i < arms.len() {
        folded = folded.union(arms[i].modes);
        i += 1;
    }
    folded
}

/// The phases any arm of a family may arrive in.
pub(crate) const fn fold_phases(arms: &[ArmMeta]) -> PhaseSet {
    let mut folded = PhaseSet::NONE;
    let mut i = 0;
    while i < arms.len() {
        folded = folded.union(arms[i].phases);
        i += 1;
    }
    folded
}

/// `None` once two arms travel opposite ways, or when there is no arm.
pub(crate) const fn fold_direction(arms: &[ArmMeta]) -> Option<Direction> {
    let Some(first) = arms.first() else {
        return None;
    };
    let first = first.direction;
    let mut i = 1;
    while i < arms.len() {
        if arms[i].direction as u8 != first as u8 {
            return None;
        }
        i += 1;
    }
    Some(first)
}

/// Whether every arm of a family declares the same routing columns, so
/// the kind-level folds lose nothing.
pub(crate) const fn arms_are_uniform(arms: &[ArmMeta]) -> bool {
    let mut i = 1;
    while i < arms.len() {
        if !arms[i].routes_like(&arms[0]) {
            return false;
        }
        i += 1;
    }
    true
}

/// A wrapper whose every arm names its row of the arm-routing table.
pub trait Directed {
    /// This wrapper's arms, in the schema's `oneof msg` order. The
    /// kind-level routing gates are folds of these tables
    /// ([`crate::MessageKind::modes`]), so a row and the gate that
    /// admits it cannot say different things.
    const ARMS: &'static [ArmMeta];

    /// The one way every arm of this wrapper travels. A family whose
    /// arms go both ways is two wrappers, so the reader's side names
    /// the type it decodes and a wrong-direction arm has no variant to
    /// land in; a table mixing directions fails to compile here.
    const DIRECTION: Direction = match fold_direction(Self::ARMS) {
        Some(direction) => direction,
        None => panic!("a wrapper's arms travel one way"),
    };

    /// Which row of [`Self::ARMS`] this arm is.
    fn arm_index(&self) -> usize;

    fn meta(&self) -> ArmMeta {
        Self::ARMS[self.arm_index()]
    }

    fn variant(&self) -> &'static str {
        self.meta().name
    }

    fn opens_stream(&self) -> bool {
        self.meta().correlation.opens_stream()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_phase_set_never_admits_the_preface() {
        for set in [
            PhaseSet::HANDSHAKE,
            PhaseSet::SETUP,
            PhaseSet::ATTACHED,
            PhaseSet::OBSERVING,
            PhaseSet::SETUP_OR_ATTACHED,
            PhaseSet::LIVE,
            PhaseSet::ANY,
        ] {
            assert!(!set.contains(Phase::Preface), "{set:?}");
        }
    }

    #[test]
    fn each_set_contains_exactly_the_members_it_names() {
        assert!(PhaseSet::HANDSHAKE.contains(Phase::Handshake));
        assert!(!PhaseSet::HANDSHAKE.contains(Phase::Setup));
        assert_eq!(
            PhaseSet::SETUP.union(PhaseSet::ATTACHED),
            PhaseSet::SETUP_OR_ATTACHED
        );
        assert_eq!(
            PhaseSet::SETUP_OR_ATTACHED.union(PhaseSet::OBSERVING),
            PhaseSet::LIVE
        );
        assert_eq!(PhaseSet::LIVE.union(PhaseSet::HANDSHAKE), PhaseSet::ANY);
        for phase in Phase::EVERY_FRAMED {
            assert!(PhaseSet::ANY.contains(phase), "{phase:?}");
        }
        assert!(!PhaseSet::LIVE.contains(Phase::Handshake));

        assert!(ModeSet::WINDOW.contains(ConnectionMode::Window));
        assert!(!ModeSet::WINDOW.contains(ConnectionMode::Ops));
        assert_eq!(
            ModeSet::WINDOW.union(ModeSet::OPS),
            ModeSet::ATTACHERS,
            "the attach-capable modes are Window and Ops"
        );
        for mode in [
            ConnectionMode::Window,
            ConnectionMode::Ops,
            ConnectionMode::Observer,
        ] {
            assert!(ModeSet::EVERY.contains(mode), "{mode:?}");
        }
    }

    #[test]
    fn a_declaration_spells_every_column() {
        let meta = ArmMeta::new(
            "Ops::Status",
            Direction::ToDaemon,
            CorrelationClass::RequestOpener,
            ModeSet::ATTACHERS,
        )
        .since(5);
        assert_eq!(
            meta.declaration(),
            "direction: DIRECTION_TO_DAEMON correlation: CORRELATION_REQUEST_OPENER \
             modes: MODE_WINDOW modes: MODE_OPS phases: PHASE_ATTACHED since: 5"
        );
    }

    #[test]
    fn a_base_arm_declares_no_since() {
        let meta = ArmMeta::new(
            "Ops::Status",
            Direction::ToDaemon,
            CorrelationClass::RequestOpener,
            ModeSet::ATTACHERS,
        );
        assert_eq!(
            meta.declaration(),
            "direction: DIRECTION_TO_DAEMON correlation: CORRELATION_REQUEST_OPENER \
             modes: MODE_WINDOW modes: MODE_OPS phases: PHASE_ATTACHED"
        );
    }
}

//! `felis-protocol`: version preface, IPC frames, message families, and connection modes.
//!
//! Protocol specification is in `docs/reference/ipc.md`.

#![cfg_attr(not(test), forbid(unsafe_code))]

pub mod base64;
pub mod build_identity;
pub mod caps;
pub mod codec;
/// Validating layer between the domain types ([`messages`]) and the
/// generated wire types ([`wire`]).
pub mod convert;
pub mod frame;
pub mod kitty_graphics;
pub mod kitty_keyboard;
pub mod kitty_text_sizing;
pub mod limits;
pub mod messages;
pub mod minor;
pub mod preface;
pub mod row;
pub mod session_prefix;
/// Generated protobuf wire types (the `felis.v1` schema); prefer the
/// domain types in [`messages`].
#[doc(hidden)]
pub mod wire;

pub use build_identity::BuildIdentity;
pub use caps::ConnectionMode;
pub use kitty_graphics::{ImageId, PlacementId};
pub use minor::{MinorGated, Requires};
pub use preface::{PROTOCOL_MAJOR, PROTOCOL_MINOR};
pub use row::RowPayload;

/// Canonical display form of a session id: 32 zero-padded lowercase
/// hex digits. Every surface that prints an id (`FELIS_SESSION_ID`,
/// CLI/JSON output, tracing) goes through this, and prefix resolution
/// matches against the same form.
#[derive(Debug, Clone, Copy)]
pub struct SessionHex(pub u128);

impl core::fmt::Display for SessionHex {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

/// Family of a frame (the `kind` header field). The numbering is the
/// schema's `FrameKind` enum (`proto/felis.proto`); prose source:
/// `docs/reference/ipc.md` "Message families". A kind outside this
/// enum is not growth to skip: the effective minor is a send-side
/// contract, so receiving one ends the connection.
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageKind {
    /// The connection handshake (`ConnToDaemonMsg` / `ConnToClientMsg`).
    Conn = 0,
    /// Client → daemon: keys, paste, mouse, resize, focus.
    Input = 1,
    /// Daemon → client: cell diffs, cursor, OSC events.
    Grid = 2,
    /// Daemon → client: image bytes + placement (Kitty graphics).
    Image = 3,
    /// This connection's session lifecycle (`SessionToDaemonMsg` /
    /// `SessionToClientMsg`).
    Session = 4,
    /// One-shot ops on named other sessions (`OpsToDaemonMsg` /
    /// `OpsToClientMsg`).
    Ops = 5,
    /// Region-export family (`RegionToDaemonMsg` / `RegionToClientMsg`).
    Region = 6,
    /// Notification observer connections (`NotifyToDaemonMsg` /
    /// `NotifyToClientMsg`).
    Notify = 7,
    /// Daemon→client pushes (`PushMsg`).
    Push = 8,
    /// Scrollback search conversation on the attached connection
    /// (`SearchToDaemonMsg` / `SearchToClientMsg`).
    Search = 9,
}

impl MessageKind {
    #[must_use]
    pub const fn from_u16(v: u16) -> Option<Self> {
        match v {
            0 => Some(Self::Conn),
            1 => Some(Self::Input),
            2 => Some(Self::Grid),
            3 => Some(Self::Image),
            4 => Some(Self::Session),
            5 => Some(Self::Ops),
            6 => Some(Self::Region),
            7 => Some(Self::Notify),
            8 => Some(Self::Push),
            9 => Some(Self::Search),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }

    /// The rows of this family's `direction` wrapper, in the schema's
    /// `oneof msg` order; empty when no arm of the family travels that
    /// way.
    pub(crate) const fn arms(self, direction: messages::Direction) -> &'static [messages::ArmMeta] {
        use messages::Directed as _;
        use messages::Direction::{ToClient, ToDaemon};
        match (self, direction) {
            (Self::Conn, ToDaemon) => messages::ConnToDaemonMsg::ARMS,
            (Self::Conn, ToClient) => messages::ConnToClientMsg::ARMS,
            (Self::Input, ToDaemon) => messages::InputMsg::ARMS,
            (Self::Grid, ToClient) => messages::GridMsg::ARMS,
            (Self::Image, ToClient) => messages::ImageMsg::ARMS,
            (Self::Session, ToDaemon) => messages::SessionToDaemonMsg::ARMS,
            (Self::Session, ToClient) => messages::SessionToClientMsg::ARMS,
            (Self::Ops, ToDaemon) => messages::OpsToDaemonMsg::ARMS,
            (Self::Ops, ToClient) => messages::OpsToClientMsg::ARMS,
            (Self::Region, ToDaemon) => messages::RegionToDaemonMsg::ARMS,
            (Self::Region, ToClient) => messages::RegionToClientMsg::ARMS,
            (Self::Notify, ToDaemon) => messages::NotifyToDaemonMsg::ARMS,
            (Self::Notify, ToClient) => messages::NotifyToClientMsg::ARMS,
            (Self::Push, ToClient) => messages::PushMsg::ARMS,
            (Self::Search, ToDaemon) => messages::SearchToDaemonMsg::ARMS,
            (Self::Search, ToClient) => messages::SearchToClientMsg::ARMS,
            (Self::Input, ToClient) | (Self::Grid | Self::Image | Self::Push, ToDaemon) => &[],
        }
    }

    /// The fold of the arm-routing table ([`messages::ArmMeta::modes`])
    /// over both directions of this family, and the driver's pre-decode
    /// filter. A kind is admitted where *any* of its arms is, so the
    /// arm check after the decode is not redundant.
    #[must_use]
    pub const fn modes(self) -> messages::ModeSet {
        messages::fold_modes(self.arms(messages::Direction::ToDaemon)).union(messages::fold_modes(
            self.arms(messages::Direction::ToClient),
        ))
    }

    /// The phases any arm of this kind may arrive in; the same fold as
    /// [`Self::modes`].
    #[must_use]
    pub const fn phases(self) -> messages::PhaseSet {
        messages::fold_phases(self.arms(messages::Direction::ToDaemon)).union(
            messages::fold_phases(self.arms(messages::Direction::ToClient)),
        )
    }

    /// `None` for the families with a wrapper for each direction, whose
    /// frames the reader decodes as its own side's wrapper.
    #[must_use]
    pub const fn sole_direction(self) -> Option<messages::Direction> {
        use messages::Direction::{ToClient, ToDaemon};
        match (
            self.arms(ToDaemon).is_empty(),
            self.arms(ToClient).is_empty(),
        ) {
            (false, true) => Some(ToDaemon),
            (true, false) => Some(ToClient),
            _ => None,
        }
    }

    /// Whether every arm of this family declares the same routing columns.
    ///
    /// When true, kind-level routing folds are exact and can be enforced
    /// without decoding individual message arms.
    #[must_use]
    pub const fn arms_are_uniform(self) -> bool {
        match self.sole_direction() {
            Some(direction) => messages::arms_are_uniform(self.arms(direction)),
            None => false,
        }
    }

    /// Whether a connection in `mode`, at `phase`, may carry any arm of
    /// this kind. The frame header is all this reads, so it runs before
    /// the body is decoded; the arm's own row decides the rest.
    #[must_use]
    pub const fn admissible(self, mode: ConnectionMode, phase: messages::Phase) -> bool {
        self.modes().contains(mode) && self.phases().contains(phase)
    }

    /// Whether this family's wrapper carries the [`messages::Correlation`]
    /// envelope at field 100 (`proto/felis.proto`).
    ///
    /// Envelopes on non-correlated families are rejected on ingress.
    #[must_use]
    pub const fn is_correlated(self) -> bool {
        match self {
            Self::Session | Self::Ops | Self::Region | Self::Notify | Self::Search => true,
            Self::Conn | Self::Input | Self::Grid | Self::Image | Self::Push => false,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Conn => "Conn",
            Self::Input => "Input",
            Self::Grid => "Grid",
            Self::Image => "Image",
            Self::Session => "Session",
            Self::Ops => "Ops",
            Self::Region => "Region",
            Self::Notify => "Notify",
            Self::Push => "Push",
            Self::Search => "Search",
        }
    }
}

impl core::fmt::Display for MessageKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl MessageKind {
    /// Every kind the wire defines. The length is part of the type, so
    /// a new kind fails to compile until it is listed here, which is
    /// what makes this the authority the schema check and the arm table
    /// in [`messages`] measure themselves against.
    pub(crate) const EVERY: [Self; 10] = [
        Self::Conn,
        Self::Input,
        Self::Grid,
        Self::Image,
        Self::Session,
        Self::Ops,
        Self::Region,
        Self::Notify,
        Self::Push,
        Self::Search,
    ];
}

const _: () = {
    let mut i = 0;
    while i < MessageKind::EVERY.len() {
        let kind = MessageKind::EVERY[i];
        match MessageKind::from_u16(kind.as_u16()) {
            Some(back) => assert!(back as u16 == kind as u16),
            None => panic!("every kind decodes from the value it encodes to"),
        }
        // A wrapper filed under the other direction would be decoded by
        // the side that sends it.
        let mut d = 0;
        let directions = [messages::Direction::ToDaemon, messages::Direction::ToClient];
        while d < directions.len() {
            let arms = kind.arms(directions[d]);
            let mut a = 0;
            while a < arms.len() {
                assert!(arms[a].direction as u8 == directions[d] as u8);
                a += 1;
            }
            d += 1;
        }
        assert!(
            !kind.arms(messages::Direction::ToDaemon).is_empty()
                || !kind.arms(messages::Direction::ToClient).is_empty(),
            "every kind carries an arm"
        );
        i += 1;
    }
};

#[cfg(test)]
mod tests {
    use super::*;

    const EVERY_KIND: [MessageKind; 10] = MessageKind::EVERY;

    /// `MessageKind` agrees value-for-value with the schema's `FrameKind`;
    /// the exhaustive `match` makes a new kind fail to compile until the
    /// schema names its counterpart.
    #[test]
    fn every_message_kind_matches_the_schema_enum() {
        use crate::wire::v1::FrameKind;
        for kind in EVERY_KIND {
            let schema = match kind {
                MessageKind::Conn => FrameKind::Conn,
                MessageKind::Input => FrameKind::Input,
                MessageKind::Grid => FrameKind::Grid,
                MessageKind::Image => FrameKind::Image,
                MessageKind::Session => FrameKind::Session,
                MessageKind::Ops => FrameKind::Ops,
                MessageKind::Region => FrameKind::Region,
                MessageKind::Notify => FrameKind::Notify,
                MessageKind::Push => FrameKind::Push,
                MessageKind::Search => FrameKind::Search,
            };
            assert_eq!(
                i32::from(kind.as_u16()),
                schema as i32,
                "{kind:?} disagrees with the schema's FrameKind"
            );
            assert_eq!(MessageKind::from_u16(schema as u16), Some(kind));
        }
        // A kind the schema gained without a Rust counterpart fails here.
        let in_schema = (0..64).filter(|v| FrameKind::try_from(*v).is_ok()).count();
        assert_eq!(
            in_schema,
            EVERY_KIND.len(),
            "felis.proto names a FrameKind that MessageKind does not"
        );
    }

    #[test]
    fn kind_wire_values_are_pinned() {
        assert_eq!(MessageKind::Conn.as_u16(), 0);
        assert_eq!(MessageKind::Input.as_u16(), 1);
        assert_eq!(MessageKind::Grid.as_u16(), 2);
        assert_eq!(MessageKind::Image.as_u16(), 3);
        assert_eq!(MessageKind::Session.as_u16(), 4);
        assert_eq!(MessageKind::Ops.as_u16(), 5);
        assert_eq!(MessageKind::Region.as_u16(), 6);
        assert_eq!(MessageKind::Notify.as_u16(), 7);
        assert_eq!(MessageKind::Push.as_u16(), 8);
        assert_eq!(MessageKind::Search.as_u16(), 9);
    }

    #[test]
    fn unknown_kind_returns_none() {
        assert_eq!(MessageKind::from_u16(99), None);
    }

    /// The correlated set is exactly the families whose wrapper declares
    /// `Correlation correlation = 100` in the schema.
    #[test]
    fn only_the_request_and_stream_families_are_correlated() {
        for kind in EVERY_KIND {
            let expected = matches!(
                kind,
                MessageKind::Session
                    | MessageKind::Ops
                    | MessageKind::Region
                    | MessageKind::Notify
                    | MessageKind::Search
            );
            assert_eq!(kind.is_correlated(), expected, "{kind:?}.is_correlated");
        }
    }

    #[test]
    fn session_hex_pins_32_char_zero_padded_lowercase() {
        assert_eq!(
            SessionHex(0xfe115).to_string(),
            "000000000000000000000000000fe115"
        );
    }
}

//! The executable half of the minor ledger: what a value costs to put
//! on the wire, in effective minors. `docs/reference/ipc.md` "The
//! minor ledger" is the prose half and carries each addition's
//! old-peer behavior; this is what the send gate reads
//! (`docs/explanation/architecture/ipc.md` "The ledger is the review gate").

use crate::caps::ConnectionMode;
use crate::messages::{
    AttachFailure, AttachRefusal, ConnToClientMsg, ConnToDaemonMsg, CreateFailure, GridMsg,
    ImageMsg, OpsToClientMsg, OpsToDaemonMsg, PushMsg, RefusalReason, ResourceKind, SearchSource,
    SearchToDaemonMsg, SessionToClientMsg, SessionToDaemonMsg, UpgradeRefusal,
};
use crate::row::RowPayload;

/// The lowest effective minor a value may be sent at, and the ledger
/// addition that raised it. The name is what a refusal reports, so it
/// spells the identifier the ledger row names rather than the Rust
/// path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Requires {
    pub minor: u16,
    pub what: &'static str,
}

impl Requires {
    /// The base schema of the major: every peer that got past the
    /// preface defines it.
    pub const BASE: Self = Self {
        minor: 0,
        what: "the base schema",
    };

    #[must_use]
    pub const fn new(minor: u16, what: &'static str) -> Self {
        Self { minor, what }
    }

    /// The stricter of two requirements. Ties keep `self`, so the
    /// outermost addition names the refusal.
    #[must_use]
    pub const fn max(self, other: Self) -> Self {
        if other.minor > self.minor {
            other
        } else {
            self
        }
    }
}

/// An added field an older peer must not skip, because skipping it
/// changes what the message asks for, paired with the test that decides
/// whether a value carries it.
pub struct GatedField<T> {
    pub requires: Requires,
    pub present: fn(&T) -> bool,
}

/// The strictest requirement `value`'s present gated fields carry.
fn field_requires<T>(value: &T, fields: &[GatedField<T>]) -> Requires {
    fields.iter().fold(Requires::BASE, |acc, field| {
        if (field.present)(value) {
            acc.max(field.requires)
        } else {
            acc
        }
    })
}

const SEARCH_TO_DAEMON_FIELDS: &[GatedField<SearchToDaemonMsg>] = &[GatedField {
    requires: crate::schema_meta::SEARCH_QUERY_SOURCE,
    present: |msg| match msg {
        SearchToDaemonMsg::Query { source, .. } => *source != SearchSource::Scrollback,
    },
}];

/// A value whose send authorization is computed rather than branched
/// on. The gate is the send boundary ([`felis-transport`'s
/// `FrameWriter`](https://docs.rs/felis-transport)), never the caller.
pub trait MinorGated {
    /// The strictest requirement this value carries, over its own arm,
    /// the closed-enum values and connection mode it names, and any
    /// row-codec version it embeds.
    fn requires(&self) -> Requires;
}

/// Every row-codec version and the minor that defined it
/// (`docs/reference/row-codec.md` "Versioning"). The tag identifies a
/// codec; this table is what authorizes putting it on the wire.
pub const ROW_CODEC_SINCE: &[(u8, u16)] = &[(1, 0)];

/// What sending `payload` costs. An unknown leading byte is this
/// build's own bug rather than a peer's, and answering
/// [`Requires::BASE`] would let it out; the highest known version is
/// the conservative answer.
#[must_use]
pub fn row_codec_requires(payload: &RowPayload) -> Requires {
    let known = payload.0.first().and_then(|version| {
        ROW_CODEC_SINCE
            .iter()
            .find_map(|(codec, minor)| (codec == version).then_some(*minor))
    });
    let minor = known.unwrap_or_else(|| {
        ROW_CODEC_SINCE
            .iter()
            .map(|(_, minor)| *minor)
            .max()
            .unwrap_or(0)
    });
    Requires::new(minor, "the row codec version")
}

impl AttachFailure {
    #[must_use]
    pub const fn since_minor(self) -> u16 {
        crate::convert::attach_failure_to_wire(self).since_minor()
    }
}

impl CreateFailure {
    #[must_use]
    pub const fn since_minor(self) -> u16 {
        crate::convert::create_failure_to_wire(self).since_minor()
    }
}

impl AttachRefusal {
    #[must_use]
    pub const fn since_minor(self) -> u16 {
        match self {
            Self::Attach(reason) => reason.since_minor(),
            Self::Create(reason) => reason.since_minor(),
        }
    }

    /// The enum the gate names, so a skew report says which vocabulary
    /// the value came from.
    const fn vocabulary(self) -> &'static str {
        match self {
            Self::Attach(_) => "AttachFailure",
            Self::Create(_) => "CreateFailure",
        }
    }
}

impl ConnectionMode {
    /// The minor that defined this mode. A mode is how a whole new
    /// surface reaches the wire (`docs/explanation/architecture/ipc.md`
    /// "Schema evolution"), so each one carries a ledger row and a
    /// client may name only the modes the effective minor defines.
    #[must_use]
    pub const fn since_minor(self) -> u16 {
        crate::convert::mode_to_wire(self).since_minor()
    }
}

impl RefusalReason {
    #[must_use]
    pub const fn since_minor(self) -> u16 {
        crate::convert::refusal_to_wire(self).since_minor()
    }
}

impl ResourceKind {
    #[must_use]
    pub const fn since_minor(self) -> u16 {
        crate::convert::resource_kind_to_wire(self).since_minor()
    }
}

impl UpgradeRefusal {
    #[must_use]
    pub const fn since_minor(self) -> u16 {
        crate::convert::upgrade_refusal_to_wire(self).since_minor()
    }
}

impl MinorGated for ConnToDaemonMsg {
    fn requires(&self) -> Requires {
        let arm = arm_requires(self);
        match self {
            Self::Hello { mode, .. } => {
                arm.max(Requires::new(mode.since_minor(), "ConnectionMode"))
            }
            Self::Cancel { .. } => arm,
        }
    }
}

impl MinorGated for ConnToClientMsg {
    fn requires(&self) -> Requires {
        let arm = arm_requires(self);
        match self {
            Self::Refused { reason, .. } => {
                arm.max(Requires::new(reason.since_minor(), "RefusalReason"))
            }
            Self::Welcome { .. } | Self::End { .. } | Self::Error { .. } => arm,
        }
    }
}

impl MinorGated for SessionToDaemonMsg {
    fn requires(&self) -> Requires {
        arm_requires(self)
    }
}

impl MinorGated for SessionToClientMsg {
    fn requires(&self) -> Requires {
        let arm = arm_requires(self);
        match self {
            Self::AttachFailed { reason, .. } => {
                arm.max(Requires::new(reason.since_minor(), reason.vocabulary()))
            }
            Self::Attached { .. } | Self::Created { .. } | Self::InputAccepted => arm,
        }
    }
}

impl MinorGated for OpsToDaemonMsg {
    fn requires(&self) -> Requires {
        arm_requires(self)
    }
}

impl MinorGated for OpsToClientMsg {
    fn requires(&self) -> Requires {
        let arm = arm_requires(self);
        match self {
            Self::StatusReply { resources, .. } => resources.iter().fold(arm, |acc, row| {
                acc.max(Requires::new(row.resource.since_minor(), "ResourceKind"))
            }),
            Self::Spawned {
                outcome: crate::messages::SpawnOutcome::Refused { reason, .. },
            } => arm.max(Requires::new(reason.since_minor(), "CreateFailure")),
            Self::UpgradeReply {
                outcome: crate::messages::UpgradeOutcome::Refused { reason, .. },
            } => arm.max(Requires::new(reason.since_minor(), "UpgradeRefusal")),
            _ => arm,
        }
    }
}

impl MinorGated for PushMsg {
    fn requires(&self) -> Requires {
        arm_requires(self)
    }
}

impl MinorGated for GridMsg {
    fn requires(&self) -> Requires {
        let arm = arm_requires(self);
        match self {
            Self::RowDelta { rows } => rows
                .iter()
                .fold(arm, |acc, (_, cells)| acc.max(row_codec_requires(cells))),
            _ => arm,
        }
    }
}

fn arm_requires(arm: &impl crate::messages::Directed) -> Requires {
    let meta = arm.meta();
    Requires::new(meta.since_minor, meta.name)
}

/// Families whose only gated additions are arms: no post-baseline
/// closed-enum value, mode or codec version rides inside one.
macro_rules! gated_by_arm {
    ($($family:ty),+ $(,)?) => {
        $(impl MinorGated for $family {
            fn requires(&self) -> Requires {
                arm_requires(self)
            }
        })+
    };
}

gated_by_arm!(
    ImageMsg,
    crate::messages::InputMsg,
    crate::messages::NotifyToDaemonMsg,
    crate::messages::NotifyToClientMsg,
    crate::messages::RegionToDaemonMsg,
    crate::messages::RegionToClientMsg,
    crate::messages::SearchToClientMsg,
);

impl MinorGated for SearchToDaemonMsg {
    fn requires(&self) -> Requires {
        arm_requires(self).max(field_requires(self, SEARCH_TO_DAEMON_FIELDS))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PROTOCOL_MINOR;
    use crate::messages::{ArmMeta, CorrelationClass, Directed, Direction, ModeSet};

    /// A stricter requirement wins whichever side it arrives on, and a
    /// tie keeps the first so the outermost addition names the refusal.
    #[test]
    fn the_strictest_requirement_wins() {
        let base = Requires::BASE;
        let five = Requires::new(5, "five");
        let other_five = Requires::new(5, "other five");
        assert_eq!(base.max(five), five);
        assert_eq!(five.max(base), five);
        assert_eq!(five.max(other_five).what, "five");
    }

    /// The arm whose row a post-release minor would carry: nothing
    /// declares it on the wire, and it stands in for the first real one.
    struct FutureArm;

    impl Directed for FutureArm {
        const ARMS: &'static [ArmMeta] = &[ArmMeta::new(
            "Future::Arm",
            Direction::ToDaemon,
            CorrelationClass::Uncorrelated,
            ModeSet::OPS,
        )
        .since(PROTOCOL_MINOR + 1)];

        fn arm_index(&self) -> usize {
            0
        }
    }

    /// The whole first-release schema is authorized at minor 0, and the
    /// gate still reads a row past it: an arm one minor ahead reaches
    /// the send boundary with its own requirement rather than the base.
    #[test]
    fn an_arm_from_a_later_minor_outranks_the_base_schema() {
        let future = arm_requires(&FutureArm);
        assert_eq!(future.minor, PROTOCOL_MINOR + 1);
        assert_eq!(future.what, "Future::Arm");
        assert_eq!(Requires::BASE.max(future), future);
    }

    /// Only a non-default source costs minor 1: the default travels as
    /// an absent field, which every peer already reads as scrollback.
    #[test]
    fn a_search_names_minor_one_only_for_a_non_default_source() {
        use crate::messages::SearchOptions;

        let query = |source| SearchToDaemonMsg::Query {
            query: "needle".into(),
            options: SearchOptions::default(),
            source,
        };
        assert_eq!(query(SearchSource::Scrollback).requires().minor, 0);
        assert_eq!(
            query(SearchSource::Visible).requires(),
            Requires::new(1, "SearchQuery.source")
        );
    }

    /// Both halves of the upgrade pair, every refusal included, cost
    /// minor 1: a daemon never answers a minor-0 peer with a reply that
    /// peer cannot decode.
    #[test]
    fn the_upgrade_pair_costs_minor_one() {
        use crate::messages::UpgradeOutcome;

        let request = OpsToDaemonMsg::Upgrade {
            successor: "/usr/bin/felis-daemon".into(),
        };
        assert_eq!(request.requires().minor, 1);
        assert_eq!(
            OpsToClientMsg::UpgradeReply {
                outcome: UpgradeOutcome::Upgrading,
            }
            .requires()
            .minor,
            1
        );
        for reason in [
            UpgradeRefusal::Unsupported,
            UpgradeRefusal::DumpVersion,
            UpgradeRefusal::ProtocolMajor,
            UpgradeRefusal::Timeout,
            UpgradeRefusal::ProbeFailed,
            UpgradeRefusal::Busy,
            UpgradeRefusal::Draining,
        ] {
            let reply = OpsToClientMsg::UpgradeReply {
                outcome: UpgradeOutcome::Refused {
                    reason,
                    detail: String::new(),
                },
            };
            assert_eq!(reply.requires().minor, 1, "{reason:?}");
        }
    }

    /// A `Hello` costs the minor that defined the mode it names, so a
    /// mode added later cannot reach a daemon that predates it. Every
    /// shipped mode is a base-schema one, so the pinning case is the
    /// synthetic row a future mode would carry.
    #[test]
    fn a_hello_costs_the_minor_that_defined_its_mode() {
        for mode in [
            ConnectionMode::Window,
            ConnectionMode::Ops,
            ConnectionMode::Observer,
        ] {
            let hello = ConnToDaemonMsg::Hello {
                mode,
                pull_paced: false,
            };
            assert_eq!(
                hello.requires().minor,
                mode.since_minor(),
                "{mode:?} must reach the send gate with its own ledger row"
            );
        }

        let future = Requires::new(PROTOCOL_MINOR + 1, "ConnectionMode");
        assert_eq!(
            Requires::BASE.max(future),
            future,
            "a mode from a later minor outranks the arm's own base requirement"
        );
    }

    /// The ledger's field rows rest on protobuf rather than the gate: a
    /// decoder that predates an added field reads the body as if the
    /// sender had left the field out, and a current decoder accepts the
    /// body an older sender writes. Pinned on the one added field, through
    /// a hand-written stand-in for the minor-0 schema.
    #[test]
    fn a_peer_that_predates_a_field_reads_it_as_left_out() {
        use prost::Message as _;

        #[derive(Clone, PartialEq, prost::Message)]
        struct VirtualPlacementMinor0 {
            #[prost(uint32, tag = "1")]
            image_id: u32,
            #[prost(uint32, tag = "2")]
            cols: u32,
            #[prost(uint32, tag = "3")]
            rows: u32,
            #[prost(sint32, tag = "4")]
            z_index: i32,
        }

        let carrying = crate::wire::v1::ImageVirtualPlacement {
            image_id: 12,
            cols: 4,
            rows: 2,
            z_index: -1,
            placement_id: Some(3),
        };
        let left_out = crate::wire::v1::ImageVirtualPlacement {
            placement_id: None,
            ..carrying
        };
        let read = |bytes: Vec<u8>| {
            VirtualPlacementMinor0::decode(bytes.as_slice()).expect("an older decoder reads it")
        };
        assert_eq!(
            read(carrying.encode_to_vec()),
            read(left_out.encode_to_vec())
        );

        let from_older_sender = crate::wire::v1::ImageMsg {
            msg: Some(crate::wire::v1::image_msg::Msg::VirtualPlacement(left_out)),
        };
        assert_eq!(
            crate::codec::decode::<ImageMsg>(from_older_sender.encode_to_vec().as_slice())
                .expect("a current decoder accepts it"),
            ImageMsg::VirtualPlacement {
                image_id: crate::ImageId(12),
                placement_id: None,
                cols: 4,
                rows: 2,
                z_index: -1,
            }
        );
    }

    /// The closed enums whose values the send gate reads, as the
    /// `MinorGated` impls above name them.
    const GATED_ENUMS: &[&str] = &[
        "AttachFailure",
        "CreateFailure",
        "RefusalReason",
        "ResourceKind",
        "UpgradeRefusal",
        "ConnectionMode",
    ];

    /// A value the schema dates past the base schema reaches an older
    /// peer unless the send gate reads its enum, so every enum with one
    /// is an enum the gate reads.
    #[test]
    fn every_enum_with_an_added_value_is_read_by_the_send_gate() {
        for name in crate::schema_meta::ANNOTATED_ENUMS {
            assert!(
                GATED_ENUMS.contains(name),
                "{name} has a value added after the base schema, but no MinorGated impl reads it"
            );
        }
    }

    /// The `Addition` cell of each ledger row in `docs/reference/ipc.md`,
    /// indexed by minor.
    fn ledger_rows() -> Vec<String> {
        let doc = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/reference/ipc.md"
        ))
        .expect("docs/reference/ipc.md beside the workspace");
        doc.split("### The minor ledger")
            .nth(1)
            .expect("ipc.md has a \"The minor ledger\" section")
            .lines()
            .skip_while(|line| !line.starts_with("| Minor |"))
            .skip(2)
            .take_while(|line| line.starts_with('|'))
            .map(|row| {
                row.trim_start_matches('|')
                    .split('|')
                    .nth(1)
                    .expect("a ledger row has an Addition cell")
                    .to_owned()
            })
            .collect()
    }

    /// A field the schema marks gated reaches an older peer only through
    /// a presence test here, so each one has exactly one, under the
    /// requirement the schema states.
    #[test]
    fn every_gated_field_of_the_schema_has_a_presence_test() {
        let gated: Vec<Requires> = SEARCH_TO_DAEMON_FIELDS
            .iter()
            .map(|field| field.requires)
            .collect();
        assert_eq!(gated, crate::schema_meta::GATED_FIELDS);
    }

    /// The prose ledger and the executable metadata must cover the same
    /// additions: the table is as long as the highest minor any arm,
    /// field, enum value, or row codec claims, and every post-baseline
    /// one is named in its own row. Forgetting either half is what lets
    /// an addition reach the wire with no authorization behind it.
    #[test]
    fn the_ledger_covers_every_executable_addition() {
        let rows = ledger_rows();
        let named = crate::schema_meta::ADDITIONS;

        let highest = named
            .iter()
            .map(|(_, minor)| *minor)
            .chain(ROW_CODEC_SINCE.iter().map(|(_, minor)| *minor))
            .max()
            .expect("the metadata names at least the base schema");
        assert_eq!(
            rows.len(),
            usize::from(highest) + 1,
            "the ledger table has {} rows for additions reaching minor {highest}",
            rows.len()
        );

        for (name, minor) in named {
            let minor = *minor;
            if minor == 0 {
                continue;
            }
            let row = &rows[usize::from(minor)];
            assert!(
                ledger_identifiers(row).contains(name),
                "minor {minor} authorizes `{name}`, but the ledger row for minor {minor} \
                 does not name it: add it to the table in docs/reference/ipc.md"
            );
        }
        for (codec, minor) in ROW_CODEC_SINCE {
            if *minor == 0 {
                continue;
            }
            assert!(
                rows[usize::from(*minor)].contains(&format!("row-codec version {codec}")),
                "minor {minor} defines row-codec version {codec}, but its ledger row does not \
                 name it"
            );
        }

        for (minor, row) in rows.iter().enumerate() {
            let minor = u16::try_from(minor).expect("the ledger is shorter than 65536 rows");
            let spans = ledger_identifiers(row);
            assert!(
                minor == 0 || !spans.is_empty(),
                "the ledger row for minor {minor} names no addition by identifier: \
                 spell it as `felis.proto` does, so the metadata check can read it"
            );
            for span in spans {
                let found = named.iter().find(|(name, _)| *name == span);
                let Some((name, claimed)) = found else {
                    panic!(
                        "the ledger row for minor {minor} names `{span}`, which no arm, \
                         field or enum value declares: give it metadata in \
                         crates/felis-protocol or drop the identifier from the row"
                    );
                };
                assert_eq!(
                    *claimed, minor,
                    "the ledger row for minor {minor} names `{span}`, but `{name}` \
                     is authorized from minor {claimed}"
                );
            }
        }
    }

    /// Every ledger identifier in `row`: an arm as `Family::Variant`, a
    /// closed-enum value as `Enum::VALUE`, a field as `Message.field`.
    /// A backticked span that is none of those is prose the row is free
    /// to carry (a bare type name, a variant of an inner enum, a command
    /// line), which the reverse check must not read as an addition.
    fn ledger_identifiers(row: &str) -> Vec<&str> {
        row.split('`')
            .skip(1)
            .step_by(2)
            .filter(|span| {
                let qualified = span
                    .split_once("::")
                    .or_else(|| span.split_once('.'))
                    .is_some_and(|(owner, member)| {
                        owner.starts_with(char::is_uppercase) && !member.is_empty()
                    });
                qualified
                    && span
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':' || c == '.')
            })
            .collect()
    }

    /// The only shipped codec version is a base-schema one, and an
    /// unknown tag answers the strictest known minor rather than the
    /// most permissive.
    #[test]
    fn a_row_codec_version_is_authorized_from_the_table() {
        assert_eq!(
            row_codec_requires(&RowPayload(vec![1])),
            Requires::new(0, "the row codec version")
        );
        let highest = ROW_CODEC_SINCE.iter().map(|(_, m)| *m).max().unwrap_or(0);
        assert_eq!(row_codec_requires(&RowPayload(vec![9])).minor, highest);
        assert_eq!(row_codec_requires(&RowPayload(Vec::new())).minor, highest);
    }
}

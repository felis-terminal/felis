//! The executable half of the minor ledger: what a value costs to put
//! on the wire, in effective minors. `docs/reference/ipc.md` "The
//! minor ledger" is the prose half and carries each addition's
//! old-peer behavior; this is what the send gate reads
//! (`docs/explanation/architecture/ipc.md` "The ledger is the review gate").

use crate::caps::ConnectionMode;
use crate::messages::{
    AttachFailure, AttachRefusal, ConnToClientMsg, ConnToDaemonMsg, CreateFailure, GridMsg,
    OpsToClientMsg, OpsToDaemonMsg, PushMsg, RefusalReason, ResourceKind, SessionToClientMsg,
    SessionToDaemonMsg, SpawnArgs,
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

    /// `field`'s requirement when `present`, [`Self::BASE`] otherwise:
    /// an omitted field costs nothing.
    #[must_use]
    pub const fn when(present: bool, field: Self) -> Self {
        if present { field } else { Self::BASE }
    }
}

/// A post-baseline field, paired with the test that decides whether a
/// value carries it. The pair is the field's one home: a `requires`
/// impl folds its owner's table instead of naming fields one by one, so
/// the list [`gated_fields`] reports is the list the gate reads.
pub struct GatedField<T> {
    pub requires: Requires,
    pub present: fn(&T) -> bool,
}

/// The strictest requirement `value`'s present fields carry.
fn field_requires<T>(value: &T, fields: &[GatedField<T>]) -> Requires {
    fields.iter().fold(Requires::BASE, |acc, field| {
        acc.max(Requires::when((field.present)(value), field.requires))
    })
}

/// Every post-baseline field the gate authorizes, named as the ledger
/// spells it, collected from the tables the `requires` impls fold.
#[must_use]
pub fn gated_fields() -> Vec<Requires> {
    fn listed<T>(fields: &[GatedField<T>]) -> impl Iterator<Item = Requires> + '_ {
        fields.iter().map(|field| field.requires)
    }
    listed(CONN_TO_DAEMON_FIELDS)
        .chain(listed(CONN_TO_CLIENT_FIELDS))
        .chain(listed(SESSION_TO_DAEMON_FIELDS))
        .chain(listed(SESSION_TO_CLIENT_FIELDS))
        .chain(listed(OPS_TO_DAEMON_FIELDS))
        .chain(listed(OPS_TO_CLIENT_FIELDS))
        .chain(listed(PUSH_FIELDS))
        .chain(listed(SESSION_INFO_FIELDS))
        .chain(listed(SPAWN_ARGS_FIELDS))
        .collect()
}

// The first public schema is the whole minor-0 base, so no field is
// post-baseline yet. The tables stay because the `requires` impls fold
// them: the first post-release field is one row here and nothing else.
const CONN_TO_DAEMON_FIELDS: &[GatedField<ConnToDaemonMsg>] = &[];
const CONN_TO_CLIENT_FIELDS: &[GatedField<ConnToClientMsg>] = &[];
const SESSION_TO_DAEMON_FIELDS: &[GatedField<SessionToDaemonMsg>] = &[];
const SESSION_TO_CLIENT_FIELDS: &[GatedField<SessionToClientMsg>] = &[];
const OPS_TO_DAEMON_FIELDS: &[GatedField<OpsToDaemonMsg>] = &[];
const OPS_TO_CLIENT_FIELDS: &[GatedField<OpsToClientMsg>] = &[];
const PUSH_FIELDS: &[GatedField<PushMsg>] = &[];
const SESSION_INFO_FIELDS: &[GatedField<crate::messages::SessionInfo>] = &[];
const SPAWN_ARGS_FIELDS: &[GatedField<SpawnArgs>] = &[];

/// A value whose send authorization is computed rather than branched
/// on. The gate is the send boundary ([`felis-transport`'s
/// `FrameWriter`](https://docs.rs/felis-transport)), never the caller.
pub trait MinorGated {
    /// The strictest requirement this value carries, over its own arm,
    /// its present fields, the closed-enum values it names, and any
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
        match self {
            Self::UnknownSession
            | Self::SessionEnding
            | Self::SessionExited
            | Self::NoMatch
            | Self::Ambiguous => 0,
        }
    }
}

impl CreateFailure {
    #[must_use]
    pub const fn since_minor(self) -> u16 {
        match self {
            Self::SpawnFailed
            | Self::GeometryOutOfRange
            | Self::SessionLimitReached
            | Self::DaemonDraining => 0,
        }
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
        match self {
            Self::Window | Self::Ops | Self::Observer => 0,
        }
    }
}

impl RefusalReason {
    #[must_use]
    pub const fn since_minor(self) -> u16 {
        match self {
            Self::Role | Self::AtCapacity | Self::UnknownMode => 0,
        }
    }
}

impl ResourceKind {
    #[must_use]
    pub const fn since_minor(self) -> u16 {
        match self {
            Self::Sessions
            | Self::ImageStoreBytes
            | Self::InFlightDecodes
            | Self::InFlightDecodeBytes
            | Self::SubscriberQueueBytes
            | Self::Connections
            | Self::PtyInputBytes => 0,
        }
    }
}

impl MinorGated for SpawnArgs {
    fn requires(&self) -> Requires {
        field_requires(self, SPAWN_ARGS_FIELDS)
    }
}

impl MinorGated for ConnToDaemonMsg {
    fn requires(&self) -> Requires {
        let arm = arm_requires(self).max(field_requires(self, CONN_TO_DAEMON_FIELDS));
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
        let arm = arm_requires(self).max(field_requires(self, CONN_TO_CLIENT_FIELDS));
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
        let arm = arm_requires(self).max(field_requires(self, SESSION_TO_DAEMON_FIELDS));
        match self {
            Self::Create { args } => arm.max(args.requires()),
            Self::Attach { .. } | Self::Detach | Self::ConfigureTheme { .. } | Self::InputFence => {
                arm
            }
        }
    }
}

impl MinorGated for SessionToClientMsg {
    fn requires(&self) -> Requires {
        let arm = arm_requires(self).max(field_requires(self, SESSION_TO_CLIENT_FIELDS));
        match self {
            Self::Attached { info } | Self::Created { info } => {
                arm.max(session_info_requires(info))
            }
            Self::AttachFailed { reason, .. } => {
                arm.max(Requires::new(reason.since_minor(), reason.vocabulary()))
            }
            Self::InputAccepted => arm,
        }
    }
}

impl MinorGated for OpsToDaemonMsg {
    fn requires(&self) -> Requires {
        let arm = arm_requires(self).max(field_requires(self, OPS_TO_DAEMON_FIELDS));
        match self {
            Self::Switch { target, .. } => arm.max(match target {
                crate::messages::SwitchTarget::Session(_) => Requires::BASE,
                crate::messages::SwitchTarget::Carrier(target) => retarget_requires(target),
            }),
            Self::Spawn { args } => arm.max(args.requires()),
            _ => arm,
        }
    }
}

impl MinorGated for OpsToClientMsg {
    fn requires(&self) -> Requires {
        let arm = arm_requires(self).max(field_requires(self, OPS_TO_CLIENT_FIELDS));
        match self {
            Self::Listed { sessions } => sessions
                .iter()
                .fold(arm, |acc, info| acc.max(session_info_requires(info))),
            Self::StatusReply { resources, .. } => resources.iter().fold(arm, |acc, row| {
                acc.max(Requires::new(row.resource.since_minor(), "ResourceKind"))
            }),
            Self::Spawned { outcome } => match outcome {
                crate::messages::SpawnOutcome::Ok { info } => arm.max(session_info_requires(info)),
                crate::messages::SpawnOutcome::Refused { reason, .. } => {
                    arm.max(Requires::new(reason.since_minor(), "CreateFailure"))
                }
            },
            _ => arm,
        }
    }
}

impl MinorGated for PushMsg {
    fn requires(&self) -> Requires {
        let arm = arm_requires(self).max(field_requires(self, PUSH_FIELDS));
        match self {
            Self::RetargetHost { target, .. } => arm.max(retarget_requires(target)),
            Self::Reattach { .. } | Self::Evicted { .. } | Self::SessionExited { .. } => arm,
        }
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

fn session_info_requires(info: &crate::messages::SessionInfo) -> Requires {
    field_requires(info, SESSION_INFO_FIELDS)
}

/// A retarget descriptor is relayed verbatim, so its landing's
/// [`SpawnArgs`] rides to the peer inside whatever carries it.
fn retarget_requires(target: &crate::messages::RetargetTarget) -> Requires {
    match &target.landing {
        crate::messages::RetargetLanding::Attach(_) => Requires::BASE,
        crate::messages::RetargetLanding::Create(args) => args.requires(),
    }
}

fn arm_requires(arm: &impl crate::messages::Directed) -> Requires {
    let meta = arm.meta();
    Requires::new(meta.since_minor, meta.name)
}

/// Families whose every addition is an arm: no post-baseline field or
/// enum value rides inside one.
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
    crate::messages::InputMsg,
    crate::messages::ImageMsg,
    crate::messages::NotifyToDaemonMsg,
    crate::messages::NotifyToClientMsg,
    crate::messages::RegionToDaemonMsg,
    crate::messages::RegionToClientMsg,
    crate::messages::SearchToDaemonMsg,
    crate::messages::SearchToClientMsg,
);

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
        assert_eq!(gated_fields(), Vec::new());
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

    /// An absent optional field costs nothing, and a table row raises
    /// the value that carries it: the fold the `requires` impls run
    /// over an empty table is the one the first post-release field
    /// joins.
    #[test]
    fn a_field_is_gated_only_when_it_is_present() {
        const FUTURE: &[GatedField<SpawnArgs>] = &[GatedField {
            requires: Requires::new(PROTOCOL_MINOR + 1, "SpawnArgs.future"),
            present: |args| args.env_base.is_some(),
        }];

        let bare = SpawnArgs::default();
        assert_eq!(bare.requires(), Requires::BASE);
        assert_eq!(field_requires(&bare, FUTURE), Requires::BASE);

        let carrying = SpawnArgs {
            env_base: Some(Vec::new()),
            ..SpawnArgs::default()
        };
        assert_eq!(carrying.requires(), Requires::BASE);
        assert_eq!(
            field_requires(&carrying, FUTURE).minor,
            PROTOCOL_MINOR + 1,
            "a table row must raise the value that carries its field"
        );
    }

    /// Every value of every gated closed enum, walked from the
    /// generated enum rather than listed: a value added to
    /// `felis.proto` reaches the caller's match without anyone
    /// remembering to append it. `_UNSPECIFIED` is 0, so the walk skips
    /// it.
    fn schema_values<W: TryFrom<i32>>() -> impl Iterator<Item = W> {
        (1i32..=256).filter_map(|raw| W::try_from(raw).ok())
    }

    /// The ledger spelling of every value of a gated closed enum,
    /// matched exhaustively over the generated enum so a new value
    /// cannot reach the wire without a row to authorize it.
    fn gated_enum_values() -> Vec<(&'static str, u16)> {
        use crate::wire::v1;

        let attach = schema_values::<v1::AttachFailure>().map(|value| match value {
            v1::AttachFailure::Unspecified => unreachable!("the walk starts past UNSPECIFIED"),
            v1::AttachFailure::UnknownSession => (
                "AttachFailure::UNKNOWN_SESSION",
                AttachFailure::UnknownSession.since_minor(),
            ),
            v1::AttachFailure::SessionEnding => (
                "AttachFailure::SESSION_ENDING",
                AttachFailure::SessionEnding.since_minor(),
            ),
            v1::AttachFailure::SessionExited => (
                "AttachFailure::SESSION_EXITED",
                AttachFailure::SessionExited.since_minor(),
            ),
            v1::AttachFailure::NoMatch => (
                "AttachFailure::NO_MATCH",
                AttachFailure::NoMatch.since_minor(),
            ),
            v1::AttachFailure::Ambiguous => (
                "AttachFailure::AMBIGUOUS",
                AttachFailure::Ambiguous.since_minor(),
            ),
        });
        let create = schema_values::<v1::CreateFailure>().map(|value| match value {
            v1::CreateFailure::Unspecified => unreachable!("the walk starts past UNSPECIFIED"),
            v1::CreateFailure::SpawnFailed => (
                "CreateFailure::SPAWN_FAILED",
                CreateFailure::SpawnFailed.since_minor(),
            ),
            v1::CreateFailure::GeometryOutOfRange => (
                "CreateFailure::GEOMETRY_OUT_OF_RANGE",
                CreateFailure::GeometryOutOfRange.since_minor(),
            ),
            v1::CreateFailure::SessionLimitReached => (
                "CreateFailure::SESSION_LIMIT_REACHED",
                CreateFailure::SessionLimitReached.since_minor(),
            ),
            v1::CreateFailure::DaemonDraining => (
                "CreateFailure::DAEMON_DRAINING",
                CreateFailure::DaemonDraining.since_minor(),
            ),
        });
        let refusal = schema_values::<v1::RefusalReason>().map(|value| match value {
            v1::RefusalReason::Unspecified => unreachable!("the walk starts past UNSPECIFIED"),
            v1::RefusalReason::Role => ("RefusalReason::ROLE", RefusalReason::Role.since_minor()),
            v1::RefusalReason::AtCapacity => (
                "RefusalReason::AT_CAPACITY",
                RefusalReason::AtCapacity.since_minor(),
            ),
            v1::RefusalReason::UnknownMode => (
                "RefusalReason::UNKNOWN_MODE",
                RefusalReason::UnknownMode.since_minor(),
            ),
        });
        let mode = schema_values::<v1::ConnectionMode>().map(|value| match value {
            v1::ConnectionMode::Unspecified => unreachable!("the walk starts past UNSPECIFIED"),
            v1::ConnectionMode::Window => (
                "ConnectionMode::WINDOW",
                ConnectionMode::Window.since_minor(),
            ),
            v1::ConnectionMode::Ops => ("ConnectionMode::OPS", ConnectionMode::Ops.since_minor()),
            v1::ConnectionMode::Observer => (
                "ConnectionMode::OBSERVER",
                ConnectionMode::Observer.since_minor(),
            ),
        });
        let resource = schema_values::<v1::ResourceKind>().map(|value| match value {
            v1::ResourceKind::Unspecified => unreachable!("the walk starts past UNSPECIFIED"),
            v1::ResourceKind::Connections => (
                "ResourceKind::CONNECTIONS",
                ResourceKind::Connections.since_minor(),
            ),
            v1::ResourceKind::Sessions => (
                "ResourceKind::SESSIONS",
                ResourceKind::Sessions.since_minor(),
            ),
            v1::ResourceKind::ImageStoreBytes => (
                "ResourceKind::IMAGE_STORE_BYTES",
                ResourceKind::ImageStoreBytes.since_minor(),
            ),
            v1::ResourceKind::InFlightDecodes => (
                "ResourceKind::IN_FLIGHT_DECODES",
                ResourceKind::InFlightDecodes.since_minor(),
            ),
            v1::ResourceKind::InFlightDecodeBytes => (
                "ResourceKind::IN_FLIGHT_DECODE_BYTES",
                ResourceKind::InFlightDecodeBytes.since_minor(),
            ),
            v1::ResourceKind::SubscriberQueueBytes => (
                "ResourceKind::SUBSCRIBER_QUEUE_BYTES",
                ResourceKind::SubscriberQueueBytes.since_minor(),
            ),
            v1::ResourceKind::PtyInputBytes => (
                "ResourceKind::PTY_INPUT_BYTES",
                ResourceKind::PtyInputBytes.since_minor(),
            ),
        });
        attach
            .chain(create)
            .chain(refusal)
            .chain(mode)
            .chain(resource)
            .collect()
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

    /// The prose ledger and the executable metadata must cover the same
    /// additions: the table is as long as the highest minor any arm,
    /// field, enum value, or row codec claims, and every post-baseline
    /// one is named in its own row. Forgetting either half is what lets
    /// an addition reach the wire with no authorization behind it.
    #[test]
    fn the_ledger_covers_every_executable_addition() {
        let rows = ledger_rows();
        let arms = crate::messages::tests::every_wrapper()
            .into_iter()
            .flat_map(|wrapper| wrapper.arms)
            .map(|arm| {
                let variant = arm
                    .name
                    .rsplit("::")
                    .next()
                    .expect("an arm name is Family::Variant");
                (variant, arm.since_minor)
            });
        let codecs = ROW_CODEC_SINCE
            .iter()
            .map(|(codec, minor)| (format!("row-codec version {codec}"), *minor));

        let listed = gated_fields();
        let fields = listed.iter().map(|field| (field.what, field.minor));

        let named: Vec<(String, u16)> = arms
            .chain(gated_enum_values())
            .chain(fields)
            .map(|(name, minor)| (name.to_owned(), minor))
            .chain(codecs)
            .collect();

        let highest = named
            .iter()
            .map(|(_, minor)| *minor)
            .max()
            .expect("the metadata names at least the base schema");
        assert_eq!(
            rows.len(),
            usize::from(highest) + 1,
            "the ledger table has {} rows for additions reaching minor {highest}",
            rows.len()
        );

        for (name, minor) in &named {
            let minor = *minor;
            if minor == 0 {
                continue;
            }
            let row = &rows[usize::from(minor)];
            assert!(
                row.contains(name.as_str()),
                "minor {minor} authorizes `{name}`, but the ledger row for minor {minor} \
                 does not name it: add it to the table in docs/reference/ipc.md"
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
                let variant = span.rsplit("::").next().unwrap_or(span);
                let found = named
                    .iter()
                    .find(|(name, _)| name == span || name == variant);
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

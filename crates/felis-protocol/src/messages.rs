//! Application message families on distinct frame kinds.
//!
//! Message families and their routing are specified in `docs/reference/ipc.md`.

use serde::{Deserialize, Serialize};

mod arm;
mod conn;
mod grid;
mod image;
mod input;
mod limits;
mod notify;
mod ops;
mod push;
mod region;
mod search;
mod session;

pub use arm::{ArmMeta, CorrelationClass, Directed, Direction, ModeSet, Phase, PhaseSet};
pub(crate) use arm::{arms_are_uniform, fold_modes, fold_phases};
pub use conn::{ConnToClientMsg, ConnToDaemonMsg, RefusalReason};
pub use grid::{
    AttentionSource, ClipboardSelection, ClipboardWrite, CursorStyle, GridMsg, ModifyOtherKeys,
    MouseProtocol, PaletteAction, PromptKind, ScrollDirection, ThemeAction, ThemeChannel,
};
pub use image::{ImageFormat, ImageMsg, ImageTarget, MAX_IMAGE_CHUNK_PAYLOAD, SourceRect};
pub use input::{
    FKey, InputMods, InputMsg, Key, KeyEvent, KeyEventKind, KeyLocation, KeyMods, MouseAction,
    MouseButton, MouseEvent, NamedKey, PromptJump,
};
pub use limits::{
    MAX_BRIDGE_LINE_BYTES, MAX_IMAGE_BYTES, MAX_IMAGE_FRAMES, MAX_KEY_CHARACTER_BYTES,
    MAX_KEY_TEXT_BYTES, MAX_PASTE_BYTES, MAX_RAW_INPUT_BYTES, MAX_REGION_REPLY_BYTES,
    MAX_RETARGET_DESCRIPTOR_BYTES, MAX_SEARCH_PATTERN_BYTES, MAX_SESSION_IMAGE_BYTES,
    MAX_SPAWN_ARGV_BYTES, MAX_SPAWN_ARGV_ENTRIES, MAX_SPAWN_PATH_BYTES, Validate, check_claim,
    check_count, check_limit,
};
pub use notify::{NotifyToClientMsg, NotifyToDaemonMsg};
pub use ops::{
    Attachment, InfoOutcome, Limit, MAX_SESSION_TAGS, MAX_TAG_BYTES, OpsToClientMsg,
    OpsToDaemonMsg, ReportScope, ResolvedId, ResourceKind, ResourceReport, ResourceUnit,
    SessionInfo, SessionNotification, SpawnOutcome, StopMode, StopOutcome, SubjectKind,
    SwitchDenied, SwitchScope, SwitchTarget,
};
pub use push::PushMsg;
pub use region::{RegionPosition, RegionToClientMsg, RegionToDaemonMsg};
pub use search::{ByteSpan, ColSpan, SearchOptions, SearchToClientMsg, SearchToDaemonMsg};
pub use session::{
    AttachFailure, AttachRefusal, AttachTarget, CreateFailure, MAX_ENV_BASE_BYTES,
    MAX_ENV_BASE_ENTRIES, SessionToClientMsg, SessionToDaemonMsg, SpawnArgs,
};

/// Admitted grid geometry. No `serde(flatten)`: it must map to a
/// distinct nested message in `felis.proto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GridDims {
    pub rows: u16,
    pub cols: u16,
    /// Window pixel width, `0` for unknown (the TIOCGWINSZ convention).
    pub pixel_w: u16,
    /// Window pixel height, `0` for unknown.
    pub pixel_h: u16,
}

/// Smallest row count a session may run at.
pub const MIN_GRID_ROWS: u16 = 1;

/// Largest row count a session may run at (REQ-605a): with
/// [`MAX_GRID_COLS`], the largest power-of-two square whose
/// steady-state cell footprint fits [`GRID_RESERVATION_BUDGET_BYTES`]
/// (`felis-grid`'s `the_geometry_bounds_fit_the_held_cell_budget`).
pub const MAX_GRID_ROWS: u16 = 2048;

/// Smallest column count a session may run at.
pub const MIN_GRID_COLS: u16 = 1;

/// Largest column count a session may run at (REQ-605a). See
/// [`MAX_GRID_ROWS`].
pub const MAX_GRID_COLS: u16 = 2048;

/// Smallest nonzero pixel extent a window may report; `0` is the
/// unknown sentinel and is never raised to this.
pub const MIN_GRID_PIXELS: u16 = 1;

/// Largest pixel extent a window may report (REQ-605a): twice a
/// dual-8K desktop's 15 360 px.
pub const MAX_GRID_PIXELS: u16 = 32_768;

/// Budget the cells one session holds must fit inside at the extreme
/// geometry (REQ-605a): the primary ring's retention window plus the
/// one viewport a screen switch parks beside it. A reflow's transient
/// peak is outside the budget by decision (session-lifecycle.md
/// "Geometry bounds").
pub const GRID_RESERVATION_BUDGET_BYTES: usize = 512 << 20;

/// Which dimension a [`GeometryRejection`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeometryAxis {
    /// [`GridDims::rows`].
    Rows,
    /// [`GridDims::cols`].
    Cols,
    /// [`GridDims::pixel_w`].
    PixelWidth,
    /// [`GridDims::pixel_h`].
    PixelHeight,
}

impl GeometryAxis {
    /// The wire field's own name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rows => "rows",
            Self::Cols => "cols",
            Self::PixelWidth => "pixel_w",
            Self::PixelHeight => "pixel_h",
        }
    }

    /// The axis's name qualified by the wire message it rides in, for
    /// the [`crate::convert::WireError`] a decode raises.
    #[must_use]
    pub const fn wire_field(self) -> &'static str {
        match self {
            Self::Rows => "GridDims.rows",
            Self::Cols => "GridDims.cols",
            Self::PixelWidth => "GridDims.pixel_w",
            Self::PixelHeight => "GridDims.pixel_h",
        }
    }

    /// The axis's inclusive `[min, max]` bound.
    #[must_use]
    pub const fn bounds(self) -> (u16, u16) {
        match self {
            Self::Rows => (MIN_GRID_ROWS, MAX_GRID_ROWS),
            Self::Cols => (MIN_GRID_COLS, MAX_GRID_COLS),
            Self::PixelWidth | Self::PixelHeight => (MIN_GRID_PIXELS, MAX_GRID_PIXELS),
        }
    }
}

impl core::fmt::Display for GeometryAxis {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One axis of a [`RequestedDims`] fell outside its bound on a path that
/// rejects rather than clamps ([`RequestedDims::admit`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{axis} {value} is outside the supported range {min}..={max}")]
pub struct GeometryRejection {
    /// Which dimension was out of range.
    pub axis: GeometryAxis,
    /// The value as the peer sent it, at full wire width.
    pub value: u32,
    /// The axis's inclusive minimum.
    pub min: u16,
    /// The axis's inclusive maximum.
    pub max: u16,
}

/// Geometry as a peer asked for it, at wire width, before admission
/// ([`SpawnArgs::dims`], [`InputMsg::Resize`]). Distinct from
/// [`GridDims`] because narrowing at decode would make an oversize
/// request connection-fatal on both paths, while create must refuse it
/// with a typed reason and resize must clamp it (REQ-605a).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RequestedDims {
    pub rows: u32,
    pub cols: u32,
    /// Requested window pixel width, `0` for unknown.
    pub pixel_w: u32,
    /// Requested window pixel height, `0` for unknown.
    pub pixel_h: u32,
}

/// `0` is a sentinel on the pixel axes alone: a window that has not
/// been mapped yet, or a headless create, genuinely has no pixel size,
/// while a create wanting the daemon's default row/column count says so
/// by omitting [`crate::messages::SpawnArgs::dims`] entirely.
const fn zero_is_sentinel(axis: GeometryAxis) -> bool {
    matches!(axis, GeometryAxis::PixelWidth | GeometryAxis::PixelHeight)
}

fn admit_axis(axis: GeometryAxis, value: u32) -> Result<u16, GeometryRejection> {
    let (min, max) = axis.bounds();
    if value == 0 && zero_is_sentinel(axis) {
        return Ok(0);
    }
    if value < u32::from(min) || value > u32::from(max) {
        return Err(GeometryRejection {
            axis,
            value,
            min,
            max,
        });
    }
    Ok(u16::try_from(value).unwrap_or(max))
}

fn clamp_axis(axis: GeometryAxis, value: u32) -> u16 {
    let (min, max) = axis.bounds();
    if value == 0 && zero_is_sentinel(axis) {
        return 0;
    }
    u16::try_from(value.clamp(u32::from(min), u32::from(max))).unwrap_or(max)
}

impl RequestedDims {
    /// Admit geometry that must already name a real size: a present
    /// `SpawnArgs.dims`, or geometry a peer announces as effective.
    ///
    /// # Errors
    /// [`GeometryRejection`] naming the first offending axis.
    pub fn admit(self) -> Result<GridDims, GeometryRejection> {
        Ok(GridDims {
            rows: admit_axis(GeometryAxis::Rows, self.rows)?,
            cols: admit_axis(GeometryAxis::Cols, self.cols)?,
            pixel_w: admit_axis(GeometryAxis::PixelWidth, self.pixel_w)?,
            pixel_h: admit_axis(GeometryAxis::PixelHeight, self.pixel_h)?,
        })
    }

    /// Admit a live resize: out-of-range axes clamp (REQ-605a). Rows and
    /// columns have no sentinel, so `0` clamps up to the minimum; pixel
    /// `0` stays unknown.
    #[must_use]
    pub fn clamp(self) -> GridDims {
        GridDims {
            rows: clamp_axis(GeometryAxis::Rows, self.rows),
            cols: clamp_axis(GeometryAxis::Cols, self.cols),
            pixel_w: clamp_axis(GeometryAxis::PixelWidth, self.pixel_w),
            pixel_h: clamp_axis(GeometryAxis::PixelHeight, self.pixel_h),
        }
    }
}

impl From<GridDims> for RequestedDims {
    fn from(d: GridDims) -> Self {
        Self {
            rows: u32::from(d.rows),
            cols: u32::from(d.cols),
            pixel_w: u32::from(d.pixel_w),
            pixel_h: u32::from(d.pixel_h),
        }
    }
}

/// A decoded desktop notification (OSC 9/99/777), relayed and never
/// popped by felis (`docs/reference/protocols/notifications.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    /// `None` for a bare-body OSC 9.
    pub title: Option<String>,
    /// May be empty (a title-only OSC 99).
    pub body: String,
    /// Urgency; `Normal` for protocols (OSC 9 / 777) that carry none.
    pub urgency: Urgency,
}

/// Region of a session's buffer serialized for [`RegionToDaemonMsg::Request`].
///
/// See `docs/explanation/data-model/scrollback.md` "Piping to an external command".
/// Variant names double as `[keymap]` config tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum RegionSource {
    /// Entire retained scrollback plus the live screen.
    Scrollback,
    /// The current viewport only.
    Visible,
    /// The most recent `OSC 133 C → D` range; no prompt marks, no-op.
    CommandOutput,
    /// The most recent `OSC 133 B → D` range; no prompt marks, no-op.
    LastCommand,
}

/// Where a window should re-dial when a retarget verb moves it off its
/// current daemon, carried by [`SwitchTarget::Carrier`] and relayed
/// verbatim as [`PushMsg::RetargetHost`]. Opaque to the daemon: the
/// client owns carriers and is the only side that dials
/// (`architecture/ipc.md` "Cross-host attach: SSH stdio").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetargetTarget {
    pub carrier: RetargetCarrier,
    pub landing: RetargetLanding,
}

/// How a [`RetargetTarget`] reaches the target daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetargetCarrier {
    /// The client's own default local socket. Carries no path: the
    /// sending daemon's idea of that path is not the receiving client's.
    DefaultLocal,
    /// A local carrier on an explicit socket path
    /// (`felis window retarget <socket>`).
    LocalEndpoint(String),
    /// The SSH-stdio carrier (`felis ssh <destination>`).
    Ssh {
        /// Passed verbatim to `ssh`; resolved against the client's
        /// `~/.ssh/config`, never parsed by felis.
        destination: String,
        /// Argument tokens spliced between `ssh` and the destination,
        /// verbatim; felis does not model `ssh`'s grammar.
        ssh_args: Vec<String>,
    },
}

impl RetargetCarrier {
    /// Diagnostics only; never parsed.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::DefaultLocal => "<local>",
            Self::LocalEndpoint(path) => path,
            Self::Ssh { destination, .. } => destination,
        }
    }
}

/// What a [`RetargetTarget`] does once its dial lands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetargetLanding {
    /// Attach to a session on the target daemon, named by a hex-id
    /// prefix the client resolves after dialing: the sending daemon
    /// cannot resolve another daemon's namespace.
    Attach(String),
    /// Create a fresh session there. An empty [`SpawnArgs::command`]
    /// runs the target daemon's `$SHELL`.
    Create(SpawnArgs),
}

/// A client-allocated request id: strictly sequential from 1 over a
/// connection's life, never reused. Non-zero because 0 is how the wire
/// spells "no request id".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RequestId(core::num::NonZeroU64);

/// A client-allocated stream id, on a sequence independent of
/// [`RequestId`]'s (both from 1, never reused). With one shared
/// sequence the daemon could not tell a late cancel of a terminated
/// stream from a fabricated one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct StreamId(core::num::NonZeroU64);

macro_rules! correlation_id {
    ($ty:ident) => {
        impl $ty {
            /// `None` for 0, which the wire uses to mean "unset".
            #[must_use]
            pub const fn new(raw: u64) -> Option<Self> {
                match core::num::NonZeroU64::new(raw) {
                    Some(n) => Some(Self(n)),
                    None => None,
                }
            }

            #[must_use]
            pub const fn get(self) -> u64 {
                self.0.get()
            }
        }

        impl core::fmt::Display for $ty {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                write!(f, "{}", self.0.get())
            }
        }
    };
}

correlation_id!(RequestId);
correlation_id!(StreamId);

/// The correlation envelope: which request or stream a frame belongs
/// to. Field 100 of the family wrapper (`proto/felis.proto`), a oneof
/// there and an exclusive enum here: an arm belongs to a point request
/// or to a stream, never to both, and its [`CorrelationClass`] says
/// which. An arm that belongs to neither carries no envelope at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Correlation {
    /// The request this frame is, or answers.
    Request(RequestId),
    /// The stream this frame opens, or is an item of.
    Stream(StreamId),
}

impl Correlation {
    /// A plain request/reply envelope.
    #[must_use]
    pub const fn request(id: RequestId) -> Self {
        Self::Request(id)
    }

    /// A stream-opening request, or one item of that stream.
    #[must_use]
    pub const fn stream(id: StreamId) -> Self {
        Self::Stream(id)
    }

    /// The request this envelope names, or `None` when it names a
    /// stream.
    #[must_use]
    pub const fn request_id(self) -> Option<RequestId> {
        match self {
            Self::Request(id) => Some(id),
            Self::Stream(_) => None,
        }
    }

    /// The stream this envelope names, or `None` when it names a
    /// request.
    #[must_use]
    pub const fn stream_id(self) -> Option<StreamId> {
        match self {
            Self::Stream(id) => Some(id),
            Self::Request(_) => None,
        }
    }
}

impl core::fmt::Display for Correlation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Request(id) => write!(f, "request {id}"),
            Self::Stream(id) => write!(f, "stream {id}"),
        }
    }
}

/// What a [`ConnToClientMsg::Error`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Subject {
    /// A request that will produce no reply.
    Request(RequestId),
    /// A stream that will produce no clean end: this is its terminal.
    Stream(StreamId),
}

impl core::fmt::Display for Subject {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Request(id) => write!(f, "request {id}"),
            Self::Stream(id) => write!(f, "stream {id}"),
        }
    }
}

/// Why a request or stream failed ([`ConnToClientMsg::Error`]). The caller
/// picks its remedy from the reason; the detail only says which value
/// provoked it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamErrorReason {
    /// Unservable as asked; re-issuing it verbatim fails the same way.
    InvalidRequest,
    /// The connection holds the maximum outstanding streams; retry
    /// once an earlier stream terminates.
    TooManyStreams,
    /// The subject went away mid-flight; nothing about the request was
    /// wrong.
    Unavailable,
    /// The daemon abandoned the stream on its own fault.
    Internal,
}

impl StreamErrorReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid request",
            Self::TooManyStreams => "too many streams",
            Self::Unavailable => "unavailable",
            Self::Internal => "internal error",
        }
    }
}

/// Desktop-notification urgency: OSC 99 `u=0/1/2`; OSC 9 and OSC 777
/// carry none and default to [`Urgency::Normal`]
/// (`docs/reference/protocols/notifications.md`). [`Urgency::as_str`]
/// and the serde `rename_all` are the same tokens, which every
/// human-facing surface prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Urgency {
    /// `u=0`.
    Low,
    /// `u=1`, and the default when a protocol carries no urgency.
    #[default]
    Normal,
    /// `u=2`.
    Critical,
}

impl Urgency {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::Critical => "critical",
        }
    }
}

/// Helpers shared by every family module's round-trip tests.
#[cfg(test)]
pub(crate) mod test_support {
    use core::fmt::Debug;

    use super::Directed;
    use crate::codec::{WireCodec, decode, encode};

    /// Round-trip a message through the protobuf codec. This is also
    /// the `convert/` coverage, which keeps no parallel corpus, only
    /// targeted decode-rejection tests.
    pub(crate) fn roundtrip<M: WireCodec + Clone + PartialEq + Debug>(msg: &M) -> M {
        let bytes = encode(msg);
        decode::<M>(&bytes).unwrap()
    }

    /// A new variant cannot ship without a round-trip case, and the row
    /// it routes to must be the row that names it: `Directed::arm_index`
    /// is the only place the mapping is written, so nothing else can
    /// catch a variant wired to its neighbour's row.
    pub(crate) fn assert_covers_every_arm<T: Directed + Debug>(cases: &[T]) {
        let covered: std::collections::BTreeSet<usize> =
            cases.iter().map(Directed::arm_index).collect();
        assert_eq!(
            covered.into_iter().collect::<Vec<_>>(),
            (0..T::ARMS.len()).collect::<Vec<_>>(),
            "case list must cover every arm at least once"
        );
        for case in cases {
            let debug = format!("{case:?}");
            let variant = debug
                .split(['(', ' '])
                .next()
                .expect("a Debug rendering opens with the variant name");
            let name = case.meta().name;
            assert_eq!(
                name.rsplit("::").next(),
                Some(variant),
                "{name} is the row of a {debug}"
            );
        }
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::{
        MAX_GRID_COLS, MAX_GRID_PIXELS, MAX_GRID_ROWS, MIN_GRID_COLS, MIN_GRID_ROWS, RequestedDims,
    };

    /// Admitted geometry either fails or lands inside REQ-605a bounds
    /// across the whole `u32` domain. Ensures that no invalid geometry
    /// quadruple can pass admission.
    #[kani::proof]
    fn admitted_geometry_is_bounded_or_refused() {
        let dims = RequestedDims {
            rows: kani::any(),
            cols: kani::any(),
            pixel_w: kani::any(),
            pixel_h: kani::any(),
        };
        if let Ok(admitted) = dims.admit() {
            assert!(admitted.rows >= MIN_GRID_ROWS && admitted.rows <= MAX_GRID_ROWS);
            assert!(admitted.cols >= MIN_GRID_COLS && admitted.cols <= MAX_GRID_COLS);
            assert!(admitted.pixel_w <= MAX_GRID_PIXELS);
            assert!(admitted.pixel_h <= MAX_GRID_PIXELS);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{
        ArmMeta, Direction, GeometryAxis, GridDims, MAX_GRID_COLS, MAX_GRID_PIXELS, MAX_GRID_ROWS,
        MIN_GRID_COLS, MIN_GRID_ROWS, ModeSet, Phase, PhaseSet, RequestedDims, Urgency,
    };
    use std::collections::BTreeSet;

    use crate::{ConnectionMode, MessageKind, preface::MINOR_LEDGER, wire::v1 as wire};

    /// The head of an arm's routing option, as `buf format` lays it
    /// out: the field declaration keeps its line and the option body
    /// follows one entry per line, closed by `}];`.
    const ARM_OPTION_OPEN: &str = " [(felis.v1.arm) = {";

    /// One wrapper message of the arm-routing table: a family's whole
    /// table when its arms travel one way, one direction of it
    /// otherwise.
    pub(crate) struct Wrapper {
        pub(crate) kind: MessageKind,
        pub(crate) direction: Direction,
        pub(crate) arms: &'static [ArmMeta],
    }

    impl Wrapper {
        /// The wrapper's message name in `felis.proto`.
        pub(crate) fn schema_name(&self) -> String {
            if self.kind.sole_direction().is_some() {
                format!("{}Msg", self.kind)
            } else {
                let direction = match self.direction {
                    Direction::ToDaemon => "ToDaemon",
                    Direction::ToClient => "ToClient",
                };
                format!("{}{direction}Msg", self.kind)
            }
        }
    }

    /// The whole arm-routing table, wrapper by wrapper, each in the
    /// schema's own oneof order.
    pub(crate) fn every_wrapper() -> Vec<Wrapper> {
        MessageKind::EVERY
            .iter()
            .flat_map(|kind| {
                [Direction::ToDaemon, Direction::ToClient]
                    .into_iter()
                    .map(|direction| Wrapper {
                        kind: *kind,
                        direction,
                        arms: kind.arms(direction),
                    })
            })
            .filter(|wrapper| !wrapper.arms.is_empty())
            .collect()
    }

    const EVERY_MODE: [ConnectionMode; 3] = [
        ConnectionMode::Window,
        ConnectionMode::Ops,
        ConnectionMode::Observer,
    ];

    /// The numbers an enum declares in `felis.proto`, in source order.
    /// `path` is `Enum` for a top-level enum or `Message.Enum` for a
    /// nested one.
    fn schema_enum_numbers(schema: &str, path: &str) -> Vec<i32> {
        let (scope, name) = match path.split_once('.') {
            Some((message, name)) => (
                schema
                    .split(&format!("message {message} {{"))
                    .nth(1)
                    .unwrap_or_else(|| panic!("the schema declares message {message}")),
                name,
            ),
            None => (schema, path),
        };
        let body = scope
            .split(&format!("enum {name} {{"))
            .nth(1)
            .unwrap_or_else(|| panic!("the schema declares enum {path}"));
        let mut numbers = Vec::new();
        for line in body.lines().map(str::trim) {
            if line == "}" {
                break;
            }
            if line.starts_with("//") || line.is_empty() {
                continue;
            }
            let (_, number) = line
                .split_once(" = ")
                .unwrap_or_else(|| panic!("enum {path} holds a value declaration: {line}"));
            numbers.push(
                number
                    .trim_end_matches(';')
                    .parse()
                    .unwrap_or_else(|_| panic!("enum {path} value is numbered: {line}")),
            );
        }
        assert!(!numbers.is_empty(), "enum {path} declares no value");
        numbers
    }

    /// The values a generated proto enum defines, by name and minus the
    /// enum's own prefix, in the schema's own order. prost emits no
    /// variant list, so the numbers to look up come from the schema
    /// text: a value added anywhere in the number space shows up here
    /// on its own, which is what makes the comparisons below bite.
    fn proto_enum_names<E>(schema: &str, path: &str, prefix: &str) -> Vec<String>
    where
        E: TryFrom<i32> + Copy + ProtoEnumName,
    {
        schema_enum_numbers(schema, path)
            .into_iter()
            .map(|number| {
                let value = E::try_from(number).ok().unwrap_or_else(|| {
                    panic!("the generated {path} is missing the schema's value {number}")
                });
                value
                    .str_name()
                    .strip_prefix(prefix)
                    .unwrap_or_else(|| panic!("{} is not a {prefix}* value", value.str_name()))
                    .to_owned()
            })
            .collect()
    }

    /// `as_str_name` is an inherent method on each generated enum
    /// rather than a trait, so [`proto_enum_names`] borrows one.
    trait ProtoEnumName {
        fn str_name(&self) -> &'static str;
    }

    impl ProtoEnumName for wire::ConnectionMode {
        fn str_name(&self) -> &'static str {
            self.as_str_name()
        }
    }

    impl ProtoEnumName for wire::arm_routing::Mode {
        fn str_name(&self) -> &'static str {
            self.as_str_name()
        }
    }

    impl ProtoEnumName for wire::arm_routing::Phase {
        fn str_name(&self) -> &'static str {
            self.as_str_name()
        }
    }

    /// One set of connection modes, spelled four ways: the wire's
    /// `ConnectionMode`, the descriptor's `ArmRouting.Mode`, the
    /// `ModeSet` tokens an arm option is compared through, and the
    /// enumeration the routing tests sweep.
    #[test]
    fn every_connection_mode_is_spelled_the_same_four_ways() {
        let schema = include_str!("../proto/felis.proto");
        let wire_modes =
            proto_enum_names::<wire::ConnectionMode>(schema, "ConnectionMode", "CONNECTION_MODE_");
        assert_eq!(
            wire_modes,
            proto_enum_names::<wire::arm_routing::Mode>(schema, "ArmRouting.Mode", "MODE_"),
            "ConnectionMode and ArmRouting.Mode name different modes"
        );
        let named: Vec<&String> = wire_modes
            .iter()
            .filter(|name| *name != "UNSPECIFIED")
            .collect();
        let tokens: Vec<String> = ModeSet::EVERY
            .tokens()
            .iter()
            .map(|token| token.to_ascii_uppercase())
            .collect();
        assert_eq!(
            tokens.iter().collect::<Vec<_>>(),
            named,
            "ModeSet::EVERY spells a different set than the schema's modes"
        );
        assert_eq!(
            EVERY_MODE.len(),
            named.len(),
            "the mode sweep enumerates a different count than the schema defines"
        );
        for (at, mode) in EVERY_MODE.iter().enumerate() {
            assert!(ModeSet::EVERY.contains(*mode), "{mode:?}");
            assert!(
                !EVERY_MODE[..at].contains(mode),
                "{mode:?} is enumerated twice"
            );
        }
    }

    /// The phases have no wire enum: nothing sends one, and the schema
    /// spells them only in `ArmRouting.Phase`. That still leaves two
    /// lists to keep together, since the options are compared through
    /// the `PhaseSet` tokens.
    #[test]
    fn every_framed_phase_is_spelled_the_same_three_ways() {
        let schema = include_str!("../proto/felis.proto");
        let schema_phases: Vec<String> =
            proto_enum_names::<wire::arm_routing::Phase>(schema, "ArmRouting.Phase", "PHASE_")
                .into_iter()
                .filter(|name| name != "UNSPECIFIED")
                .collect();
        let tokens: Vec<String> = PhaseSet::ANY
            .tokens()
            .iter()
            .map(|token| token.to_ascii_uppercase())
            .collect();
        assert_eq!(
            tokens, schema_phases,
            "PhaseSet::ANY spells a different set than ArmRouting.Phase"
        );
        assert_eq!(
            Phase::EVERY_FRAMED.len(),
            schema_phases.len(),
            "the framed-phase ladder is a different length than ArmRouting.Phase"
        );
        // The preface carries bytes, not frames, so it is the one phase
        // with no schema token and no membership in any set.
        assert_eq!(Phase::EVERY.len(), Phase::EVERY_FRAMED.len() + 1);
        assert!(!PhaseSet::ANY.contains(Phase::Preface));
    }

    /// The schema's routing wrappers and [`MessageKind`] are two
    /// authorities on what the wire carries, so a surface added to one
    /// and not the other cannot reach a peer unnoticed.
    #[test]
    fn the_arm_table_covers_every_routing_family() {
        let tabled: BTreeSet<String> = every_wrapper().iter().map(Wrapper::schema_name).collect();
        // A message is a routing wrapper exactly when it holds the
        // `oneof msg` the frame body decodes into.
        let schema = include_str!("../proto/felis.proto");
        let in_schema: BTreeSet<String> = schema
            .split("\nmessage ")
            .skip(1)
            .filter_map(|block| {
                let (head, body) = block.split_once(" {")?;
                body.contains("\n  oneof msg {").then(|| head.to_owned())
            })
            .collect();
        assert_eq!(
            in_schema, tabled,
            "felis.proto declares a routing family the arm table does not"
        );
    }

    /// A kind whose wrapper `reserved`s field 100 has no envelope for
    /// an arm to be classified against, so every one of its arms must
    /// be `Uncorrelated`. The converse does not hold: `Session` carries
    /// the slot, its attach and create arms leave it unset, and only the
    /// input fence pair correlates.
    #[test]
    fn no_arm_of_an_uncorrelated_kind_declares_a_correlation_class() {
        use super::CorrelationClass;
        for Wrapper { kind, arms, .. } in every_wrapper() {
            if kind.is_correlated() {
                continue;
            }
            for arm in arms {
                assert_eq!(
                    arm.correlation,
                    CorrelationClass::Uncorrelated,
                    "{} ({kind}) declares {:?}, but {kind} carries no envelope",
                    arm.name,
                    arm.correlation
                );
            }
        }
    }

    /// No arm may be legal nowhere, and none may claim a minor this
    /// build does not speak.
    #[test]
    fn every_arm_is_reachable_under_some_mode_and_phase() {
        for Wrapper { kind, arms, .. } in every_wrapper() {
            for arm in arms {
                assert!(
                    EVERY_MODE.iter().any(|mode| arm.modes.contains(*mode)),
                    "{} ({kind}) is legal on no connection mode",
                    arm.name
                );
                assert!(
                    Phase::EVERY_FRAMED
                        .iter()
                        .any(|phase| arm.phases.contains(*phase)),
                    "{} ({kind}) is legal in no phase",
                    arm.name
                );
                // The ledger ends at `PROTOCOL_MINOR` (a const assert in
                // `preface`), so a recorded minor is also one this
                // build can encode.
                assert!(
                    MINOR_LEDGER
                        .iter()
                        .any(|(minor, _)| *minor == arm.since_minor),
                    "{} claims minor {}, which the ledger does not record",
                    arm.name,
                    arm.since_minor
                );
            }
        }
    }

    /// One arm as `felis.proto` declares it: the field name, its number,
    /// and its `(felis.v1.arm)` option normalized to the one-line form
    /// [`ArmMeta::declaration`] emits.
    #[derive(Debug, PartialEq)]
    struct DeclaredArm {
        field: String,
        tag: u32,
        declaration: String,
    }

    /// Every arm of one wrapper message, in schema order.
    fn declared_arms(schema: &str, wrapper: &str) -> Vec<DeclaredArm> {
        let head = format!("message {wrapper} {{");
        let body = schema
            .split(&head)
            .nth(1)
            .unwrap_or_else(|| panic!("the schema declares {head}"));
        let oneof = body
            .split("oneof msg {")
            .nth(1)
            .unwrap_or_else(|| panic!("{wrapper} has a `oneof msg` block"));
        let lines: Vec<&str> = oneof.lines().map(str::trim).collect();
        let mut declared = Vec::new();
        let mut at = 0;
        while at < lines.len() {
            let line = lines[at];
            at += 1;
            if line == "}" {
                break;
            }
            if line.starts_with("//") || line.is_empty() {
                continue;
            }
            let head = line.strip_suffix(ARM_OPTION_OPEN).unwrap_or_else(|| {
                panic!("{wrapper} arm carries no `(felis.v1.arm)` option: {line}")
            });
            let (field, tag) = head
                .rsplit_once('=')
                .and_then(|(field, tail)| {
                    let field = field.split_whitespace().nth(1)?;
                    Some((field.to_owned(), tail.trim().parse().ok()?))
                })
                .unwrap_or_else(|| panic!("{wrapper} arm without a field number: {line}"));
            let mut entries: Vec<&str> = Vec::new();
            loop {
                let entry = lines.get(at).unwrap_or_else(|| {
                    panic!("{wrapper} field {tag} leaves its arm option unclosed")
                });
                at += 1;
                if *entry == "}];" {
                    break;
                }
                // An entry may be annotated, and the annotation is not
                // part of the row: a comment beside a `since:` reads the
                // same to protoc and to a peer.
                let entry = entry.split("//").next().unwrap_or(entry).trim();
                if entry.is_empty() {
                    continue;
                }
                entries.push(entry);
            }
            declared.push(DeclaredArm {
                field,
                tag,
                declaration: entries.join(" "),
            });
        }
        declared
    }

    /// The schema field an arm rides in: its variant in snake case.
    fn field_of(arm: &ArmMeta) -> String {
        let variant = arm
            .name
            .rsplit("::")
            .next()
            .expect("an arm name is Family::Variant");
        let mut field = String::new();
        for (at, c) in variant.chars().enumerate() {
            if c.is_ascii_uppercase() && at > 0 {
                field.push('_');
            }
            field.push(c.to_ascii_lowercase());
        }
        field
    }

    /// A comment inside an option block annotates the row without
    /// changing it, so the schema keeps the freedom to explain an entry
    /// where the entry is.
    #[test]
    fn an_annotated_arm_option_declares_the_row_its_entries_do() {
        let bare = "message OpsToDaemonMsg {\n  oneof msg {\n    OpsList list = 1 [(felis.v1.arm) = {\n\
             direction: DIRECTION_TO_DAEMON\ncorrelation: CORRELATION_REQUEST_OPENER\n\
             modes: MODE_OPS\nphases: PHASE_SETUP\nsince: 1\n}];\n  }\n}";
        let annotated = "message OpsToDaemonMsg {\n  oneof msg {\n    // The roster query.\n\
             OpsList list = 1 [(felis.v1.arm) = {\n      direction: DIRECTION_TO_DAEMON\n\
             // Every Ops verb is a request.\n      correlation: CORRELATION_REQUEST_OPENER\n\
             \n      modes: MODE_OPS\n      phases: PHASE_SETUP\n      since: 1 // Added in 1.1.\n\
             }];\n  }\n}";
        assert_eq!(
            declared_arms(annotated, "OpsToDaemonMsg"),
            declared_arms(bare, "OpsToDaemonMsg"),
            "an annotated option declares a different row than the same option bare"
        );
    }

    /// `felis.proto` restates every arm's row in its `(felis.v1.arm)`
    /// option, so a peer that cannot read Rust reads the same matrix
    /// out of the descriptor rather than out of a comment.
    #[test]
    fn the_schema_declares_the_same_arm_table() {
        let schema = include_str!("../proto/felis.proto");
        for wrapper in every_wrapper() {
            let name = wrapper.schema_name();
            let declared: Vec<(String, String)> = declared_arms(schema, &name)
                .into_iter()
                .map(|arm| (arm.field, arm.declaration))
                .collect();
            let want: Vec<(String, String)> = wrapper
                .arms
                .iter()
                .map(|arm| (field_of(arm), arm.declaration()))
                .collect();
            assert_eq!(
                declared, want,
                "felis.proto's {name} arm options disagree with the ArmMeta table"
            );
        }
    }

    /// A family's arms share one number sequence across its wrappers, so
    /// the bytes of an arm decode as no arm of the other direction's
    /// wrapper, and the driver can still name a wrong-direction arm
    /// rather than read it as a neighbor.
    #[test]
    fn a_family_numbers_its_arms_once_across_both_directions() {
        let schema = include_str!("../proto/felis.proto");
        for kind in MessageKind::EVERY {
            let mut tags: Vec<u32> = every_wrapper()
                .iter()
                .filter(|wrapper| wrapper.kind == kind)
                .flat_map(|wrapper| {
                    let declared = declared_arms(schema, &wrapper.schema_name());
                    let in_order: Vec<u32> = declared.iter().map(|arm| arm.tag).collect();
                    let mut sorted = in_order.clone();
                    sorted.sort_unstable();
                    assert_eq!(
                        in_order,
                        sorted,
                        "{} declares its arms out of number order",
                        wrapper.schema_name()
                    );
                    in_order
                })
                .collect();
            tags.sort_unstable();
            let count = u32::try_from(tags.len()).expect("a family has few arms");
            assert_eq!(
                tags,
                (1..=count).collect::<Vec<_>>(),
                "{kind}'s arms do not number 1..={count} once across its wrappers"
            );
        }
    }

    /// The driver finds an arm by its field number without decoding, so
    /// each row's number has to be the one the schema gives that arm.
    #[test]
    fn every_arm_row_carries_its_schema_field_number() {
        let schema = include_str!("../proto/felis.proto");
        for wrapper in every_wrapper() {
            let declared = declared_arms(schema, &wrapper.schema_name());
            let numbers = crate::codec::arm_fields(wrapper.kind, wrapper.direction);
            assert_eq!(numbers.len(), wrapper.arms.len());
            for (arm, number) in wrapper.arms.iter().zip(numbers) {
                let field = field_of(arm);
                let tag = declared
                    .iter()
                    .find(|declared| declared.field == field)
                    .unwrap_or_else(|| panic!("{} declares {field}", wrapper.schema_name()))
                    .tag;
                assert_eq!(*number, tag, "{} is field {tag}", arm.name);
            }
        }
    }

    /// The preface admits no frame at all, so no arm may name it and
    /// the schema has no token for that phase. The driver's half is
    /// `no_frame_of_any_kind_survives_the_preface`.
    #[test]
    fn the_schema_spells_no_arm_into_the_preface() {
        let schema = include_str!("../proto/felis.proto");
        let declarations = schema
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with("//") && line.ends_with(ARM_OPTION_OPEN))
            .count();
        for line in schema.lines() {
            let Some(phase) = line.trim().strip_prefix("phases: ") else {
                continue;
            };
            assert!(
                [
                    "PHASE_HANDSHAKE",
                    "PHASE_SETUP",
                    "PHASE_ATTACHED",
                    "PHASE_OBSERVING"
                ]
                .contains(&phase),
                "an arm option names the phase {phase}, which admits no frame"
            );
        }
        assert!(
            declarations
                >= every_wrapper()
                    .into_iter()
                    .map(|wrapper| wrapper.arms.len())
                    .sum::<usize>(),
            "every arm of every family carries an option"
        );
        assert!(!PhaseSet::ANY.contains(Phase::Preface));
    }

    #[test]
    fn urgency_as_str_spells_each_level_in_snake_case() {
        assert_eq!(Urgency::Low.as_str(), "low");
        assert_eq!(Urgency::Normal.as_str(), "normal");
        assert_eq!(Urgency::Critical.as_str(), "critical");
    }

    /// An oversize create is a typed rejection naming the axis, over
    /// the full wire domain (REQ-605a).
    #[test]
    fn a_create_past_the_maximum_is_rejected_with_the_offending_axis() {
        for raw in [u32::from(MAX_GRID_ROWS) + 1, 65_536, u32::MAX] {
            let err = RequestedDims {
                rows: raw,
                cols: 80,
                pixel_w: 0,
                pixel_h: 0,
            }
            .admit()
            .expect_err("a create past the row maximum must be refused");
            assert_eq!(err.axis, GeometryAxis::Rows);
            assert_eq!(err.value, raw);
            assert_eq!(err.max, MAX_GRID_ROWS);
        }
    }

    #[test]
    fn each_axis_rejects_under_its_own_name() {
        let over = u32::from(MAX_GRID_PIXELS) + 1;
        let cases = [
            (
                RequestedDims {
                    rows: 24,
                    cols: u32::from(MAX_GRID_COLS) + 1,
                    pixel_w: 0,
                    pixel_h: 0,
                },
                GeometryAxis::Cols,
            ),
            (
                RequestedDims {
                    rows: 24,
                    cols: 80,
                    pixel_w: over,
                    pixel_h: 0,
                },
                GeometryAxis::PixelWidth,
            ),
            (
                RequestedDims {
                    rows: 24,
                    cols: 80,
                    pixel_w: 0,
                    pixel_h: over,
                },
                GeometryAxis::PixelHeight,
            ),
            (
                RequestedDims {
                    rows: 0,
                    cols: 80,
                    pixel_w: 0,
                    pixel_h: 0,
                },
                GeometryAxis::Rows,
            ),
            (
                RequestedDims {
                    rows: 24,
                    cols: 0,
                    pixel_w: 0,
                    pixel_h: 0,
                },
                GeometryAxis::Cols,
            ),
        ];
        for (req, axis) in cases {
            assert_eq!(req.admit().expect_err("out of range").axis, axis);
        }
    }

    #[test]
    fn the_maxima_themselves_are_accepted() {
        let req = RequestedDims {
            rows: u32::from(MAX_GRID_ROWS),
            cols: u32::from(MAX_GRID_COLS),
            pixel_w: u32::from(MAX_GRID_PIXELS),
            pixel_h: u32::from(MAX_GRID_PIXELS),
        };
        assert_eq!(
            req.admit().expect("the maxima are inside the bounds"),
            GridDims {
                rows: MAX_GRID_ROWS,
                cols: MAX_GRID_COLS,
                pixel_w: MAX_GRID_PIXELS,
                pixel_h: MAX_GRID_PIXELS,
            }
        );
    }

    /// The pixel axes keep their unknown sentinel; a headless create
    /// names rows and columns and no pixel size at all.
    #[test]
    fn admission_keeps_the_pixel_sentinel_but_not_a_zero_cell_axis() {
        assert_eq!(
            RequestedDims {
                rows: 24,
                cols: 80,
                pixel_w: 0,
                pixel_h: 0,
            }
            .admit()
            .expect("unknown pixel extents are admissible"),
            GridDims {
                rows: 24,
                cols: 80,
                pixel_w: 0,
                pixel_h: 0,
            }
        );
        assert_eq!(
            RequestedDims::default()
                .admit()
                .expect_err("zero rows is not a size")
                .axis,
            GeometryAxis::Rows,
        );
    }

    #[test]
    fn a_resize_past_the_maximum_clamps_instead_of_failing() {
        for raw in [65_536, u32::MAX] {
            assert_eq!(
                RequestedDims {
                    rows: raw,
                    cols: raw,
                    pixel_w: raw,
                    pixel_h: raw,
                }
                .clamp(),
                GridDims {
                    rows: MAX_GRID_ROWS,
                    cols: MAX_GRID_COLS,
                    pixel_w: MAX_GRID_PIXELS,
                    pixel_h: MAX_GRID_PIXELS,
                }
            );
        }
    }

    /// A `0` row or column count clamps up to the minimum; pixel `0`
    /// stays the unknown sentinel.
    #[test]
    fn a_resize_clamps_the_cell_axes_up_but_leaves_pixels_unknown() {
        assert_eq!(
            RequestedDims::default().clamp(),
            GridDims {
                rows: MIN_GRID_ROWS,
                cols: MIN_GRID_COLS,
                pixel_w: 0,
                pixel_h: 0,
            }
        );
    }
}

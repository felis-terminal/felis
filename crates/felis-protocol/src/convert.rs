//! Anti-corruption layer between domain types and protobuf wire types.
//!
//! Validates schema rules that protobuf cannot express on ingress.
//! See `docs/explanation/architecture/ipc.md`.

// A truncating cast here is always a wire-width bug: proto-widened
// integers must come back through [`narrow`], whose error names the field.
#![deny(clippy::cast_possible_truncation)]

use crate::messages;
use crate::wire::v1;

/// Ways a received wire value fails to name a valid domain value.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// A `msg` oneof or nested data-carrying oneof was `None`.
    #[error("wire oneof {0} was absent")]
    MissingOneof(&'static str),
    /// A nested message field the domain treats as mandatory was `None`.
    #[error("required wire field {0} was absent")]
    MissingField(&'static str),
    /// A proto enum field held its `_UNSPECIFIED = 0` sentinel, which
    /// has no domain counterpart.
    #[error("wire enum {0} was UNSPECIFIED")]
    UnspecifiedEnum(&'static str),
    /// A proto enum field held an integer naming no known variant.
    #[error("wire enum {field} held unknown value {value}")]
    UnknownEnum {
        /// The field whose raw value did not decode.
        field: &'static str,
        /// The raw integer read off the wire.
        value: i32,
    },
    /// A `Correlation` body set both ids. The schema's oneof cannot
    /// encode that, so the sender did not build it from the schema.
    #[error("correlation named both a request and a stream")]
    AmbiguousCorrelation,
    /// A body named an arm of each direction of one family: prost reads
    /// either wrapper past the other's arm as an unknown field, so the
    /// body has no one meaning.
    #[error("body named {to_daemon} and {to_client}, arms of opposite directions")]
    MixedDirections {
        /// The client→daemon arm the body named.
        to_daemon: &'static str,
        /// The daemon→client arm the body named.
        to_client: &'static str,
    },
    /// A session-id `bytes` field was not exactly 16 bytes.
    #[error("session id was {0} bytes, expected 16")]
    BadSessionId(usize),
    /// A bitflags field held bits this build defines no flag for. A
    /// sender may only use what the effective minor defines
    /// ([`crate::preface`]), so this is a corrupt frame, not a newer peer.
    #[error("wire field {field} held undefined bits {bits:#010x}")]
    UndefinedBits {
        /// The bitflags field whose raw value carried unknown bits.
        field: &'static str,
        /// The undefined bits alone, with the known ones masked off.
        bits: u32,
    },
    /// A field carried more than its documented per-operation limit
    /// (REQ-105a). Raised on both sides: by the sender before it
    /// encodes, and by the receiver after it decodes.
    #[error("wire field {field} carried {len} {unit}, over the {cap} limit")]
    OverLimit {
        /// The field whose payload was over its cap.
        field: &'static str,
        /// The payload's size, in the unit the cap is stated in.
        len: usize,
        /// The limit itself.
        cap: usize,
        /// What `len` and `cap` count: `"bytes"` for a payload, or
        /// `"entries"` / `"frames"` for item counts.
        unit: &'static str,
    },
    /// A proto-widened numeric field held a value outside the domain
    /// type's range. Rejected rather than wrapped: a wrapped value is a
    /// valid-looking wrong value.
    #[error("wire field {field} value {value} is out of the domain range")]
    OutOfRange {
        /// The field whose value did not fit.
        field: &'static str,
        /// The raw integer read off the wire.
        value: u32,
    },
    /// A field carried a value its own schema comment excludes: a
    /// `google.protobuf.Timestamp` outside the range that type defines,
    /// or a `0` for a 1-based counter. Neither is a value a
    /// sender that built the frame from the schema can produce, so it is
    /// refused rather than repaired.
    #[error("wire field {0} held a value the schema excludes")]
    MalformedField(&'static str),
}

fn narrow<T: TryFrom<u32>>(field: &'static str, raw: u32) -> Result<T, WireError> {
    T::try_from(raw).map_err(|_| WireError::OutOfRange { field, value: raw })
}

/// The `_UNSPECIFIED` sentinel decodes cleanly here; the per-enum
/// `TryFrom` rejects it.
fn decode_enum<W: TryFrom<i32>>(field: &'static str, raw: i32) -> Result<W, WireError> {
    W::try_from(raw).map_err(|_| WireError::UnknownEnum { field, value: raw })
}

/// Big-endian is the frozen cross-language contract for session ids.
fn id_to_bytes(id: u128) -> Vec<u8> {
    id.to_be_bytes().to_vec()
}

fn id_from_bytes(bytes: Vec<u8>) -> Result<u128, WireError> {
    let arr: [u8; 16] = bytes
        .try_into()
        .map_err(|v: Vec<u8>| WireError::BadSessionId(v.len()))?;
    Ok(u128::from_be_bytes(arr))
}

pub(crate) fn rgb_to_wire((r, g, b): (u8, u8, u8)) -> v1::Rgb {
    v1::Rgb {
        r: u32::from(r),
        g: u32::from(g),
        b: u32::from(b),
    }
}

pub(crate) fn rgb_from_wire(rgb: v1::Rgb) -> Result<(u8, u8, u8), WireError> {
    Ok((
        narrow("Rgb.r", rgb.r)?,
        narrow("Rgb.g", rgb.g)?,
        narrow("Rgb.b", rgb.b)?,
    ))
}

fn opt_rgb_to_wire(rgb: Option<(u8, u8, u8)>) -> Option<v1::Rgb> {
    rgb.map(rgb_to_wire)
}

fn opt_rgb_from_wire(rgb: Option<v1::Rgb>) -> Result<Option<(u8, u8, u8)>, WireError> {
    rgb.map(rgb_from_wire).transpose()
}

/// Requested `GridDims` cross unnarrowed for the caller's admission
/// policy to bound.
impl From<v1::GridDims> for messages::RequestedDims {
    fn from(d: v1::GridDims) -> Self {
        Self {
            rows: d.rows,
            cols: d.cols,
            pixel_w: d.pixel_w,
            pixel_h: d.pixel_h,
        }
    }
}

/// A requested `GridDims` on a field that must carry one; `None` is a
/// truncated peer.
fn requested_dims_from_wire(
    dims: Option<v1::GridDims>,
    field: &'static str,
) -> Result<messages::RequestedDims, WireError> {
    Ok(dims.ok_or(WireError::MissingField(field))?.into())
}

fn dims_from_wire(
    dims: Option<v1::GridDims>,
    field: &'static str,
) -> Result<messages::GridDims, WireError> {
    messages::GridDims::try_from(dims.ok_or(WireError::MissingField(field))?)
}

/// `From`/`TryFrom` pair plus `i32`-field helpers for a data-free enum;
/// `TryFrom<wire>` rejects the `_UNSPECIFIED` sentinel.
macro_rules! data_free_enum {
    (
        $field:literal, $to_i32:ident, $from_i32:ident, $domain:path, $wire:path,
        { $($dv:ident => $wv:ident),+ $(,)? }
    ) => {
        impl From<$domain> for $wire {
            fn from(v: $domain) -> Self {
                match v { $( <$domain>::$dv => Self::$wv ),+ }
            }
        }
        impl TryFrom<$wire> for $domain {
            type Error = WireError;
            fn try_from(v: $wire) -> Result<Self, Self::Error> {
                match v {
                    <$wire>::Unspecified => Err(WireError::UnspecifiedEnum($field)),
                    $( <$wire>::$wv => Ok(Self::$dv) ),+
                }
            }
        }
        fn $to_i32(v: $domain) -> i32 {
            <$wire>::from(v) as i32
        }
        fn $from_i32(raw: i32) -> Result<$domain, WireError> {
            <$domain>::try_from(decode_enum::<$wire>($field, raw)?)
        }
    };
}

data_free_enum!("Urgency", urgency_to_i32, urgency_from_i32, messages::Urgency, v1::Urgency, {
    Low => Low,
    Normal => Normal,
    Critical => Critical,
});

data_free_enum!("RegionSource", region_source_to_i32, region_source_from_i32, messages::RegionSource, v1::RegionSource, {
    Scrollback => Scrollback,
    Visible => Visible,
    CommandOutput => CommandOutput,
    LastCommand => LastCommand,
});

impl From<messages::RequestedDims> for v1::GridDims {
    fn from(d: messages::RequestedDims) -> Self {
        Self {
            rows: d.rows,
            cols: d.cols,
            pixel_w: d.pixel_w,
            pixel_h: d.pixel_h,
        }
    }
}

impl From<messages::GridDims> for v1::GridDims {
    fn from(d: messages::GridDims) -> Self {
        Self {
            rows: u32::from(d.rows),
            cols: u32::from(d.cols),
            pixel_w: u32::from(d.pixel_w),
            pixel_h: u32::from(d.pixel_h),
        }
    }
}

/// Announced geometry is admitted at wire width, not narrowed: a
/// receiver sizes a grid from these four numbers, so `65535 × 65535`
/// surviving the narrowing is a 4.3-billion-cell allocation ordered by
/// a 20-byte frame. The REQ-605a bounds are the same ones the daemon
/// admits a create against, so nothing downstream re-checks.
impl TryFrom<v1::GridDims> for messages::GridDims {
    type Error = WireError;
    fn try_from(d: v1::GridDims) -> Result<Self, Self::Error> {
        messages::RequestedDims::from(d)
            .admit()
            .map_err(|rejection| WireError::OutOfRange {
                field: rejection.axis.wire_field(),
                value: rejection.value,
            })
    }
}

impl From<&messages::Notification> for v1::Notification {
    fn from(n: &messages::Notification) -> Self {
        Self {
            title: n.title.clone(),
            body: n.body.clone(),
            urgency: urgency_to_i32(n.urgency),
        }
    }
}

impl TryFrom<v1::Notification> for messages::Notification {
    type Error = WireError;
    fn try_from(n: v1::Notification) -> Result<Self, Self::Error> {
        Ok(Self {
            title: n.title,
            body: n.body,
            urgency: urgency_from_i32(n.urgency)?,
        })
    }
}

/// `0` is how the wire spells "unset".
pub(crate) fn stream_id_from_wire(
    field: &'static str,
    raw: u64,
) -> Result<messages::StreamId, WireError> {
    messages::StreamId::new(raw).ok_or(WireError::MissingField(field))
}

pub(crate) fn request_id_from_wire(
    field: &'static str,
    raw: u64,
) -> Result<messages::RequestId, WireError> {
    messages::RequestId::new(raw).ok_or(WireError::MissingField(field))
}

impl From<messages::Correlation> for v1::Correlation {
    fn from(c: messages::Correlation) -> Self {
        use v1::correlation::Id;
        Self {
            id: Some(match c {
                messages::Correlation::Request(id) => Id::RequestId(id.get()),
                messages::Correlation::Stream(id) => Id::StreamId(id.get()),
            }),
        }
    }
}

/// An envelope naming no id is rejected: a sender that means
/// "uncorrelated" omits the field entirely.
impl TryFrom<v1::Correlation> for messages::Correlation {
    type Error = WireError;
    fn try_from(c: v1::Correlation) -> Result<Self, Self::Error> {
        use v1::correlation::Id;
        match c.id {
            Some(Id::RequestId(raw)) => Ok(Self::Request(request_id_from_wire(
                "Correlation.request_id",
                raw,
            )?)),
            Some(Id::StreamId(raw)) => Ok(Self::Stream(stream_id_from_wire(
                "Correlation.stream_id",
                raw,
            )?)),
            None => Err(WireError::MissingOneof("Correlation.id")),
        }
    }
}

impl From<&messages::RetargetCarrier> for v1::retarget_target::Carrier {
    fn from(c: &messages::RetargetCarrier) -> Self {
        use messages::RetargetCarrier as C;
        match c {
            C::DefaultLocal => Self::DefaultLocal(v1::RetargetLocal {}),
            C::LocalEndpoint(path) => Self::LocalEndpoint(path.clone()),
            C::Ssh {
                destination,
                ssh_args,
            } => Self::Ssh(v1::SshEndpoint {
                destination: destination.clone(),
                ssh_args: ssh_args.clone(),
            }),
        }
    }
}

impl From<v1::retarget_target::Carrier> for messages::RetargetCarrier {
    fn from(c: v1::retarget_target::Carrier) -> Self {
        use v1::retarget_target::Carrier as W;
        match c {
            W::DefaultLocal(_) => Self::DefaultLocal,
            W::LocalEndpoint(path) => Self::LocalEndpoint(path),
            W::Ssh(e) => Self::Ssh {
                destination: e.destination,
                ssh_args: e.ssh_args,
            },
        }
    }
}

impl From<&messages::RetargetTarget> for v1::RetargetTarget {
    fn from(t: &messages::RetargetTarget) -> Self {
        use messages::RetargetLanding as L;
        use v1::retarget_target::Landing as W;
        Self {
            carrier: Some((&t.carrier).into()),
            landing: Some(match &t.landing {
                L::Attach(prefix) => W::Attach(prefix.clone()),
                L::Create(args) => W::Create(args.into()),
            }),
        }
    }
}

impl TryFrom<v1::RetargetTarget> for messages::RetargetTarget {
    type Error = WireError;
    fn try_from(t: v1::RetargetTarget) -> Result<Self, Self::Error> {
        use v1::retarget_target::Landing as W;
        let landing = match t
            .landing
            .ok_or(WireError::MissingOneof("RetargetTarget.landing"))?
        {
            W::Attach(prefix) => messages::RetargetLanding::Attach(prefix),
            W::Create(args) => messages::RetargetLanding::Create(args.try_into()?),
        };
        Ok(Self {
            carrier: t
                .carrier
                .ok_or(WireError::MissingOneof("RetargetTarget.carrier"))?
                .into(),
            landing,
        })
    }
}

// After `data_free_enum!`: a `macro_rules!` macro is only in scope for
// modules declared after its definition.
mod conn;
mod grid;
mod image;
mod input;
mod notify;
mod ops;
mod push;
mod region;
mod search;
mod session;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::{GridMsg, InputMsg, MouseButton};

    /// Session ids ride the wire big-endian; pinned directly, since a
    /// round trip passes under any self-consistent order.
    #[test]
    fn session_id_is_big_endian() {
        let id: u128 = 0x0102_0304_0506_0708_090A_0B0C_0D0E_0F10;
        let bytes = id_to_bytes(id);
        assert_eq!(
            bytes,
            (1u8..=16).collect::<Vec<u8>>(),
            "high byte must come first"
        );
        assert_eq!(id_from_bytes(bytes).unwrap(), id);
    }

    /// An `OpsSwitch` carrying no scope is refused rather than decoded
    /// to a scope that would move a window.
    #[test]
    fn an_absent_switch_scope_is_rejected() {
        use crate::messages::OpsToDaemonMsg;
        let wire = v1::OpsToDaemonMsg {
            msg: Some(v1::ops_to_daemon_msg::Msg::Switch(v1::OpsSwitch {
                from_prefix: "cafe".to_owned(),
                target: Some(v1::SwitchTarget {
                    target: Some(v1::switch_target::Target::Session("beef".to_owned())),
                }),
                scope: None,
            })),
            correlation: None,
        };
        let err = OpsToDaemonMsg::try_from(wire).unwrap_err();
        assert!(matches!(err, WireError::MissingField("OpsSwitch.scope")));
    }

    /// A present but empty `SwitchScope` names no arm and is refused
    /// with the oneof's own error.
    #[test]
    fn an_empty_switch_scope_oneof_is_rejected() {
        use crate::messages::OpsToDaemonMsg;
        let wire = v1::OpsToDaemonMsg {
            msg: Some(v1::ops_to_daemon_msg::Msg::Switch(v1::OpsSwitch {
                from_prefix: "cafe".to_owned(),
                target: Some(v1::SwitchTarget {
                    target: Some(v1::switch_target::Target::Session("beef".to_owned())),
                }),
                scope: Some(v1::SwitchScope { scope: None }),
            })),
            correlation: None,
        };
        let err = OpsToDaemonMsg::try_from(wire).unwrap_err();
        assert!(matches!(err, WireError::MissingOneof("SwitchScope.scope")));
    }

    /// An `OpsStop` carrying no mode is refused rather than decoded to
    /// the safe posture, which is an arm of its own.
    #[test]
    fn an_unset_stop_mode_is_rejected_rather_than_read_as_if_empty() {
        use crate::messages::OpsToDaemonMsg;
        let stop = |mode: Option<v1::ops_stop::Mode>| v1::OpsToDaemonMsg {
            msg: Some(v1::ops_to_daemon_msg::Msg::Stop(v1::OpsStop { mode })),
            correlation: None,
        };
        assert!(matches!(
            OpsToDaemonMsg::try_from(stop(None)),
            Err(WireError::MissingOneof("OpsStop.mode"))
        ));
        assert!(matches!(
            OpsToDaemonMsg::try_from(stop(Some(v1::ops_stop::Mode::IfEmpty(v1::StopIfEmpty {})))),
            Ok(OpsToDaemonMsg::Stop {
                mode: messages::StopMode::IfEmpty
            })
        ));
    }

    /// A wrong-length session id is rejected with the actual length.
    #[test]
    fn short_session_id_is_rejected() {
        let err = id_from_bytes(vec![0; 8]).unwrap_err();
        assert!(matches!(err, WireError::BadSessionId(8)));
    }

    /// The `_UNSPECIFIED` sentinel is a decode error, not a default.
    #[test]
    fn unspecified_enum_is_rejected() {
        assert!(matches!(
            urgency_from_i32(v1::Urgency::Unspecified as i32),
            Err(WireError::UnspecifiedEnum("Urgency"))
        ));
    }

    #[test]
    fn unknown_enum_value_is_rejected() {
        assert!(matches!(
            urgency_from_i32(99),
            Err(WireError::UnknownEnum {
                field: "Urgency",
                value: 99
            })
        ));
    }

    /// An unknown or unspecified `ModeFlags` tag fails the decode rather
    /// than reading as `Off`.
    #[test]
    fn out_of_range_mode_flag_tags_are_rejected() {
        let flags = |mouse: i32, modify: i32| v1::GridMsg {
            msg: Some(v1::grid_msg::Msg::ModeFlags(v1::GridModeFlags {
                bracketed_paste: false,
                alt_screen: false,
                mouse_protocol: mouse,
                application_cursor: false,
                modify_other_keys: modify,
                application_keypad: false,
                win32_input_mode: false,
                reverse_video: false,
            })),
        };
        let off = (
            v1::MouseProtocol::Off as i32,
            v1::ModifyOtherKeys::Off as i32,
        );

        assert!(matches!(
            GridMsg::try_from(flags(7, off.1)),
            Err(WireError::UnknownEnum {
                field: "MouseProtocol",
                value: 7
            })
        ));
        assert!(matches!(
            GridMsg::try_from(flags(v1::MouseProtocol::Unspecified as i32, off.1)),
            Err(WireError::UnspecifiedEnum("MouseProtocol"))
        ));
        assert!(matches!(
            GridMsg::try_from(flags(off.0, 9)),
            Err(WireError::UnknownEnum {
                field: "ModifyOtherKeys",
                value: 9
            })
        ));
        assert!(matches!(
            GridMsg::try_from(flags(off.0, v1::ModifyOtherKeys::Unspecified as i32)),
            Err(WireError::UnspecifiedEnum("ModifyOtherKeys"))
        ));
    }

    /// A `GridPaletteColor` must name both an in-range index and an
    /// explicit action.
    #[test]
    fn malformed_palette_color_is_rejected_rather_than_read_as_a_reset() {
        let palette = |index: u32, action: Option<v1::grid_palette_color::Action>| v1::GridMsg {
            msg: Some(v1::grid_msg::Msg::PaletteColor(v1::GridPaletteColor {
                index,
                action,
            })),
        };
        let set = || {
            Some(v1::grid_palette_color::Action::Set(v1::Rgb {
                r: 1,
                g: 2,
                b: 3,
            }))
        };

        assert!(matches!(
            GridMsg::try_from(palette(3, None)),
            Err(WireError::MissingOneof("GridPaletteColor.action"))
        ));
        assert!(matches!(
            GridMsg::try_from(palette(256, set())),
            Err(WireError::OutOfRange {
                field: "PaletteColor.index",
                value: 256
            })
        ));
        assert!(matches!(
            GridMsg::try_from(palette(255, set())),
            Ok(GridMsg::PaletteColor { index: 255, .. })
        ));
    }

    /// A `GridThemeColor` must name an explicit action.
    #[test]
    fn a_theme_color_without_an_action_is_rejected_rather_than_read_as_a_reset() {
        let theme = |action: Option<v1::grid_theme_color::Action>| v1::GridMsg {
            msg: Some(v1::grid_msg::Msg::ThemeColor(v1::GridThemeColor {
                channel: v1::ThemeChannel::Background as i32,
                action,
            })),
        };

        assert!(matches!(
            GridMsg::try_from(theme(None)),
            Err(WireError::MissingOneof("GridThemeColor.action"))
        ));
        assert!(matches!(
            GridMsg::try_from(theme(Some(v1::grid_theme_color::Action::Reset(
                v1::ThemeReset {}
            )))),
            Ok(GridMsg::ThemeColor {
                action: messages::ThemeAction::Reset,
                ..
            })
        ));
        assert!(matches!(
            GridMsg::try_from(theme(Some(v1::grid_theme_color::Action::Set(v1::Rgb {
                r: 1,
                g: 2,
                b: 3
            })))),
            Ok(GridMsg::ThemeColor {
                action: messages::ThemeAction::Set { rgb: (1, 2, 3) },
                ..
            })
        ));
    }

    /// A wire value beyond the domain width is refused, not wrapped.
    /// Requested geometry is absent on purpose: it decodes wide
    /// (`requested_geometry_crosses_the_layer_unnarrowed`).
    #[test]
    fn out_of_range_narrowing_is_rejected() {
        let link = v1::GridMsg {
            msg: Some(v1::grid_msg::Msg::Hyperlink(v1::GridHyperlink {
                id: 65537,
                anchor: None,
                uri: "https://example.com".into(),
            })),
        };
        assert!(matches!(
            GridMsg::try_from(link),
            Err(WireError::OutOfRange {
                field: "Hyperlink.id",
                value: 65537
            })
        ));

        let theme = v1::GridMsg {
            msg: Some(v1::grid_msg::Msg::ThemeColor(v1::GridThemeColor {
                channel: v1::ThemeChannel::Background as i32,
                action: Some(v1::grid_theme_color::Action::Set(v1::Rgb {
                    r: 300,
                    g: 0,
                    b: 0,
                })),
            })),
        };
        assert!(matches!(
            GridMsg::try_from(theme),
            Err(WireError::OutOfRange {
                field: "Rgb.r",
                value: 300
            })
        ));
    }

    fn announced(rows: u32, cols: u32, pixel_w: u32, pixel_h: u32) -> v1::GridMsg {
        v1::GridMsg {
            msg: Some(v1::grid_msg::Msg::Size(v1::GridSize {
                dims: Some(v1::GridDims {
                    rows,
                    cols,
                    pixel_w,
                    pixel_h,
                }),
            })),
        }
    }

    /// Announced geometry is admitted against the REQ-605a bounds at
    /// wire width, at each boundary and one past it. The failing case
    /// is why: `65535 × 65535` narrows cleanly to `u16` and would size
    /// a 4.3-billion-cell grid from a 20-byte frame.
    #[test]
    fn announced_geometry_is_admitted_at_the_req_605a_bounds() {
        assert!(matches!(
            GridMsg::try_from(announced(2048, 2048, 0, 32_768)),
            Ok(GridMsg::Size {
                dims: messages::GridDims {
                    rows: 2048,
                    cols: 2048,
                    pixel_w: 0,
                    pixel_h: 32_768,
                }
            })
        ));

        for (dims, field, value) in [
            (announced(2049, 80, 0, 0), "GridDims.rows", 2049),
            (announced(24, 2049, 0, 0), "GridDims.cols", 2049),
            (announced(65_535, 65_535, 0, 0), "GridDims.rows", 65_535),
            (announced(u32::MAX, 80, 0, 0), "GridDims.rows", u32::MAX),
            (announced(24, 80, 32_769, 0), "GridDims.pixel_w", 32_769),
            (announced(24, 80, 0, u32::MAX), "GridDims.pixel_h", u32::MAX),
        ] {
            let err = GridMsg::try_from(dims).unwrap_err();
            assert!(
                matches!(err, WireError::OutOfRange { field: f, value: v } if f == field && v == value),
                "{field}={value} was admitted: {err:?}",
            );
        }
    }

    /// Zero rows or columns is a claim no admission produces, so it is
    /// refused (REQ-605a: only effective geometry is reported).
    #[test]
    fn announced_geometry_has_no_zero_sentinel_for_cells() {
        assert!(matches!(
            GridMsg::try_from(announced(0, 80, 0, 0)),
            Err(WireError::OutOfRange {
                field: "GridDims.rows",
                value: 0
            })
        ));
        assert!(matches!(
            GridMsg::try_from(announced(24, 0, 0, 0)),
            Err(WireError::OutOfRange {
                field: "GridDims.cols",
                value: 0
            })
        ));
    }

    /// Requested geometry reaches the daemon at the width it was sent
    /// at, over the whole `uint32` domain (REQ-605a).
    #[test]
    fn requested_geometry_crosses_the_layer_unnarrowed() {
        for raw in [65_536, u32::MAX] {
            let resize = v1::InputMsg {
                msg: Some(v1::input_msg::Msg::Resize(v1::InputResize {
                    dims: Some(v1::GridDims {
                        rows: raw,
                        cols: raw,
                        pixel_w: raw,
                        pixel_h: raw,
                    }),
                })),
            };
            assert_eq!(
                InputMsg::try_from(resize).unwrap(),
                InputMsg::Resize {
                    dims: messages::RequestedDims {
                        rows: raw,
                        cols: raw,
                        pixel_w: raw,
                        pixel_h: raw,
                    }
                },
            );

            let create = v1::SessionToDaemonMsg {
                msg: Some(v1::session_to_daemon_msg::Msg::Create(v1::SessionCreate {
                    args: Some(v1::SpawnArgs {
                        dims: Some(v1::GridDims {
                            rows: raw,
                            cols: 80,
                            pixel_w: 0,
                            pixel_h: 0,
                        }),
                        ..v1::SpawnArgs::default()
                    }),
                })),
                correlation: None,
            };
            let messages::SessionToDaemonMsg::Create { args } =
                messages::SessionToDaemonMsg::try_from(create).unwrap()
            else {
                panic!("a Create must decode as a Create");
            };
            assert_eq!(args.dims.expect("a present dims decodes present").rows, raw);
        }
    }

    /// Absence is how a create asks for the daemon default, so it is
    /// the one geometry field a decode must not treat as truncation.
    #[test]
    fn a_create_without_dims_decodes_as_absent() {
        let create = v1::SessionToDaemonMsg {
            msg: Some(v1::session_to_daemon_msg::Msg::Create(v1::SessionCreate {
                args: Some(v1::SpawnArgs::default()),
            })),
            correlation: None,
        };
        let messages::SessionToDaemonMsg::Create { args } =
            messages::SessionToDaemonMsg::try_from(create).unwrap()
        else {
            panic!("a Create must decode as a Create");
        };
        assert_eq!(args.dims, None);
    }

    /// An absent resize geometry is still a truncated peer, not a 0x0
    /// request.
    #[test]
    fn a_resize_without_dims_is_still_rejected() {
        let resize = v1::InputMsg {
            msg: Some(v1::input_msg::Msg::Resize(v1::InputResize { dims: None })),
        };
        assert!(matches!(
            InputMsg::try_from(resize),
            Err(WireError::MissingField("InputResize.dims"))
        ));
    }

    /// An unset `MouseButton` oneof names the field rather than
    /// defaulting to the left button.
    #[test]
    fn mouse_button_without_an_arm_is_rejected() {
        assert!(matches!(
            MouseButton::try_from(v1::MouseButton { button: None }),
            Err(WireError::MissingOneof("MouseButton.button"))
        ));
    }

    #[test]
    fn a_retarget_without_a_landing_is_rejected() {
        let no_landing = v1::RetargetTarget {
            carrier: Some(v1::retarget_target::Carrier::LocalEndpoint("/s".into())),
            landing: None,
        };
        assert!(matches!(
            messages::RetargetTarget::try_from(no_landing),
            Err(WireError::MissingOneof("RetargetTarget.landing"))
        ));
    }

    /// The default-local carrier is an arm of its own, so an unset one
    /// is a peer this decoder cannot read rather than a default.
    #[test]
    fn a_retarget_without_a_carrier_is_rejected() {
        let bare = v1::RetargetTarget {
            carrier: None,
            landing: Some(v1::retarget_target::Landing::Attach("ab12".into())),
        };
        assert!(matches!(
            messages::RetargetTarget::try_from(bare),
            Err(WireError::MissingOneof("RetargetTarget.carrier"))
        ));

        let local = v1::RetargetTarget {
            carrier: Some(v1::retarget_target::Carrier::DefaultLocal(
                v1::RetargetLocal {},
            )),
            landing: Some(v1::retarget_target::Landing::Attach("ab12".into())),
        };
        let decoded = messages::RetargetTarget::try_from(local).unwrap();
        assert_eq!(decoded.carrier, messages::RetargetCarrier::DefaultLocal);
        assert_eq!(
            decoded.landing,
            messages::RetargetLanding::Attach("ab12".into())
        );
    }

    /// Carrier and landing vary independently: the full 3x2 product
    /// round-trips.
    #[test]
    fn every_carrier_and_landing_pairing_round_trips() {
        let carriers = [
            messages::RetargetCarrier::DefaultLocal,
            messages::RetargetCarrier::LocalEndpoint("/run/felis/alt.sock".into()),
            messages::RetargetCarrier::Ssh {
                destination: "user@devbox".into(),
                ssh_args: vec!["-p".into(), "2222".into()],
            },
        ];
        let landings = [
            messages::RetargetLanding::Attach("3f9c".into()),
            messages::RetargetLanding::Create(messages::SpawnArgs {
                command: "htop".into(),
                ..messages::SpawnArgs::default()
            }),
        ];
        for carrier in &carriers {
            for landing in &landings {
                let domain = messages::RetargetTarget {
                    carrier: carrier.clone(),
                    landing: landing.clone(),
                };
                let wire = v1::RetargetTarget::from(&domain);
                assert_eq!(
                    messages::RetargetTarget::try_from(wire).unwrap(),
                    domain,
                    "{domain:?}"
                );
            }
        }
    }
}

//! Body codec between domain messages and protobuf frame bodies.
//!
//! Protobuf binary is the only wire encoding; `packed_cells` format is
//! specified in `docs/reference/row-codec.md`.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CodecError {
    /// A protobuf body did not decode as its wire message.
    #[error("protobuf decode: {0}")]
    Prost(#[from] prost::DecodeError),
    /// A decoded wire message named no valid domain value (a missing
    /// oneof, an `_UNSPECIFIED` enum, a wrong-length session id).
    #[error("wire conversion: {0}")]
    Wire(#[from] crate::convert::WireError),
}

/// Encode a domain message's protobuf body. Borrows: the daemon fans
/// one message out to many peers.
#[must_use]
pub fn encode<M: WireCodec>(msg: &M) -> Vec<u8> {
    use prost::Message as _;
    msg.to_wire().encode_to_vec()
}

/// Decodes a protobuf body into its domain message.
///
/// # Errors
/// Returns [`CodecError::Prost`] on invalid protobuf bytes, or
/// [`CodecError::Wire`] on malformed domain values or limit violations.
pub fn decode<M: WireCodec>(bytes: impl Body) -> Result<M, CodecError> {
    let wire = bytes.decode_wire::<M::Wire>()?;
    let msg = M::from_wire(wire)?;
    msg.validate()?;
    Ok(msg)
}

/// What a decode accepts as a body. With a [`bytes::Bytes`] body prost
/// aliases `bytes`-typed fields (the image chunk payload) instead of
/// copying them out.
pub trait Body: Sized {
    /// Hand the body to prost, consuming it.
    fn decode_wire<W: prost::Message + Default>(self) -> Result<W, prost::DecodeError>;
}

impl Body for bytes::Bytes {
    fn decode_wire<W: prost::Message + Default>(self) -> Result<W, prost::DecodeError> {
        W::decode(self)
    }
}

impl Body for &bytes::Bytes {
    fn decode_wire<W: prost::Message + Default>(self) -> Result<W, prost::DecodeError> {
        W::decode(self.clone())
    }
}

impl Body for &[u8] {
    fn decode_wire<W: prost::Message + Default>(self) -> Result<W, prost::DecodeError> {
        W::decode(self)
    }
}

impl Body for &Vec<u8> {
    fn decode_wire<W: prost::Message + Default>(self) -> Result<W, prost::DecodeError> {
        W::decode(self.as_slice())
    }
}

impl<const N: usize> Body for &[u8; N] {
    fn decode_wire<W: prost::Message + Default>(self) -> Result<W, prost::DecodeError> {
        W::decode(&self[..])
    }
}

/// Encode a domain message with its correlation envelope attached.
#[must_use]
pub fn encode_correlated<M: Correlated>(
    msg: &M,
    correlation: crate::messages::Correlation,
) -> Vec<u8> {
    use prost::Message as _;
    let mut wire = msg.to_wire();
    M::set_correlation(&mut wire, correlation);
    wire.encode_to_vec()
}

/// Field 100 correlation slot (`proto/felis.proto`).
///
/// Held as raw repeated bytes to verify exactly which tags appeared;
/// standard prost decoding resolves oneofs as last-field-wins.
#[derive(Clone, PartialEq, prost::Message)]
struct CorrelationSlot {
    #[prost(bytes = "vec", repeated, tag = "100")]
    correlation: Vec<Vec<u8>>,
}

/// Which of the envelope's two tags a body actually wrote, with the
/// value each last carried.
#[derive(Default)]
struct CorrelationTags {
    request_id: Option<u64>,
    stream_id: Option<u64>,
}

impl TryFrom<CorrelationTags> for crate::messages::Correlation {
    type Error = crate::convert::WireError;
    fn try_from(tags: CorrelationTags) -> Result<Self, Self::Error> {
        match (tags.request_id, tags.stream_id) {
            // Ambiguity is decided by presence, not by the surviving
            // values: `request_id=7, stream_id=1, request_id=0` reads
            // last-wins as a lone stream envelope, so a value-based
            // check would admit the encoding it exists to refuse.
            (Some(_), Some(_)) => Err(crate::convert::WireError::AmbiguousCorrelation),
            (Some(raw), None) => Ok(Self::Request(crate::convert::request_id_from_wire(
                "Correlation.request_id",
                raw,
            )?)),
            (None, Some(raw)) => Ok(Self::Stream(crate::convert::stream_id_from_wire(
                "Correlation.stream_id",
                raw,
            )?)),
            (None, None) => Err(crate::convert::WireError::MissingOneof("Correlation.id")),
        }
    }
}

fn decode_error(msg: &'static str) -> prost::DecodeError {
    // prost 0.14 deprecated `DecodeError::new` (it was `doc(hidden)` and
    // meant for internal use) but it remains the only public constructor
    // for a custom message; the crate has no public `DecodeErrorKind`
    // surface. Keep the call isolated so the deprecation is allowed in
    // one place and `cargo clippy -- -D warnings` stays green.
    #[allow(deprecated)]
    {
        prost::DecodeError::new(msg)
    }
}

fn read_varint(bytes: &mut &[u8]) -> Result<u64, prost::DecodeError> {
    let mut value = 0u64;
    for shift in (0..64u32).step_by(7) {
        let (&byte, rest) = bytes
            .split_first()
            .ok_or_else(|| decode_error("truncated varint"))?;
        *bytes = rest;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(decode_error("overlong varint"))
}

const VARINT: u64 = 0;
const FIXED64: u64 = 1;
const LENGTH_DELIMITED: u64 = 2;
const FIXED32: u64 = 5;

/// Steps over one field's value; a group is refused, since proto3 has
/// none and no felis message declares one.
fn skip_value(bytes: &mut &[u8], wire_type: u64) -> Result<(), prost::DecodeError> {
    match wire_type {
        VARINT => read_varint(bytes).map(|_value| ()),
        FIXED64 => skip_bytes(bytes, 8),
        LENGTH_DELIMITED => {
            let len = usize::try_from(read_varint(bytes)?)
                .map_err(|_| decode_error("oversized field"))?;
            skip_bytes(bytes, len)
        }
        FIXED32 => skip_bytes(bytes, 4),
        _ => Err(decode_error("group wire type")),
    }
}

fn skip_bytes(bytes: &mut &[u8], len: usize) -> Result<(), prost::DecodeError> {
    if bytes.len() < len {
        return Err(decode_error("truncated field"));
    }
    *bytes = &bytes[len..];
    Ok(())
}

/// A tag written more than once keeps its last value, as a canonical
/// reader resolves it, but that it was written at all is what survives.
fn parse_correlation(mut bytes: &[u8]) -> Result<CorrelationTags, prost::DecodeError> {
    let mut tags = CorrelationTags::default();
    while !bytes.is_empty() {
        let key = read_varint(&mut bytes)?;
        let (field, wire_type) = (key >> 3, key & 7);
        // A wire type the schema's field does not have is a decode
        // error for a canonical reader too; accepting it would be felis
        // reading bytes no other implementation can.
        if matches!(field, 1 | 2) && wire_type != VARINT {
            return Err(decode_error("Correlation id is not a varint"));
        }
        if wire_type == VARINT {
            let value = read_varint(&mut bytes)?;
            match field {
                1 => tags.request_id = Some(value),
                2 => tags.stream_id = Some(value),
                _ => {}
            }
        } else {
            skip_value(&mut bytes, wire_type)?;
        }
    }
    Ok(tags)
}

/// Extracts the correlation envelope from any frame body, or `None` if omitted.
///
/// # Errors
/// Returns [`CodecError::Prost`] on invalid protobuf, or [`CodecError::Wire`]
/// if the envelope does not contain exactly one identifier.
pub fn peek_correlation(
    body: impl Body,
) -> Result<Option<crate::messages::Correlation>, CodecError> {
    let slot = body.decode_wire::<CorrelationSlot>()?;
    if slot.correlation.is_empty() {
        return Ok(None);
    }
    let tags = parse_correlation(&slot.correlation.concat())?;
    Ok(Some(tags.try_into()?))
}

/// A family whose wrapper carries the [`crate::messages::Correlation`]
/// envelope.
pub trait Correlated: WireCodec {
    /// Stamp the envelope onto an already-built wire message.
    fn set_correlation(wire: &mut Self::Wire, correlation: crate::messages::Correlation);
}

/// Pairs a domain message wrapper with its generated wire type; the
/// conversions themselves live in [`crate::convert`].
pub trait WireCodec: Sized {
    type Wire: prost::Message + Default;
    /// The frame kind this wrapper rides.
    const KIND: crate::MessageKind;
    /// The field number of each arm of `oneof msg`, row for row with
    /// [`crate::messages::Directed::ARMS`].
    const ARM_FIELDS: &'static [u32];
    /// Infallible: a domain value is always representable on the wire.
    fn to_wire(&self) -> Self::Wire;
    fn from_wire(wire: Self::Wire) -> Result<Self, crate::convert::WireError>;
    /// Whether a decoded wire value set any arm of this wrapper's
    /// `oneof msg`. Unset is what the other half of a split family
    /// decodes to, since the two halves number their arms apart.
    fn names_an_arm(wire: &Self::Wire) -> bool;
    /// The family's per-operation limits (REQ-105a).
    ///
    /// # Errors
    /// [`crate::convert::WireError::OverLimit`] naming the first field
    /// over its cap.
    fn validate(&self) -> Result<(), crate::convert::WireError>;
}

macro_rules! wire_codec {
    ($($domain:ident => $kind:ident [$($field:literal),+]),+ $(,)?) => {
        $(impl WireCodec for crate::messages::$domain {
            type Wire = crate::wire::v1::$domain;
            const KIND: crate::MessageKind = crate::MessageKind::$kind;
            const ARM_FIELDS: &'static [u32] = &[$($field),+];
            fn to_wire(&self) -> Self::Wire {
                self.into()
            }
            fn from_wire(wire: Self::Wire) -> Result<Self, crate::convert::WireError> {
                wire.try_into()
            }
            fn names_an_arm(wire: &Self::Wire) -> bool {
                wire.msg.is_some()
            }
            fn validate(&self) -> Result<(), crate::convert::WireError> {
                <Self as crate::messages::Validate>::validate(self)
            }
        }
        const _: () = assert!(
            <crate::messages::$domain as WireCodec>::ARM_FIELDS.len()
                == <crate::messages::$domain as crate::messages::Directed>::ARMS.len()
        );)+
    };
}

wire_codec!(
    ConnToDaemonMsg => Conn [1, 4],
    ConnToClientMsg => Conn [2, 3, 5, 6],
    InputMsg => Input [1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
    GridMsg => Grid [
        1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21
    ],
    ImageMsg => Image [1, 2, 3, 4, 5, 6, 7, 8, 9],
    SessionToDaemonMsg => Session [1, 2, 3, 4, 8],
    SessionToClientMsg => Session [5, 6, 7, 9],
    OpsToDaemonMsg => Ops [1, 3, 5, 7, 9, 11, 13, 15, 17],
    OpsToClientMsg => Ops [2, 4, 6, 8, 10, 12, 14, 16, 18],
    RegionToDaemonMsg => Region [1, 3],
    RegionToClientMsg => Region [2, 4, 5],
    NotifyToDaemonMsg => Notify [1],
    NotifyToClientMsg => Notify [2, 3, 4],
    PushMsg => Push [1, 2, 3, 4],
    SearchToDaemonMsg => Search [1],
    SearchToClientMsg => Search [2],
);

/// `Conn` is absent on purpose: its `Cancel` / `End` / `Error` arms
/// name their subject inline, because a terminal that lost its stream
/// id is malformed, which an optional envelope field cannot express.
macro_rules! correlated {
    ($($domain:ty),+ $(,)?) => {
        $(impl Correlated for $domain {
            fn set_correlation(
                wire: &mut Self::Wire,
                correlation: crate::messages::Correlation,
            ) {
                wire.correlation = Some(correlation.into());
            }
        })+
    };
}

correlated!(
    crate::messages::SessionToDaemonMsg,
    crate::messages::SessionToClientMsg,
    crate::messages::OpsToDaemonMsg,
    crate::messages::OpsToClientMsg,
    crate::messages::RegionToDaemonMsg,
    crate::messages::RegionToClientMsg,
    crate::messages::NotifyToDaemonMsg,
    crate::messages::NotifyToClientMsg,
    crate::messages::SearchToDaemonMsg,
    crate::messages::SearchToClientMsg,
);

/// The routing row of the arm `body` carries, read as the `direction`
/// half of `kind`: the per-frame lookup for a reader that holds a kind
/// and a side rather than a type. `None` when no arm of `kind` travels
/// that way.
pub fn arm_of(
    kind: crate::MessageKind,
    direction: crate::messages::Direction,
    body: impl Body,
) -> Option<Result<crate::messages::ArmMeta, CodecError>> {
    use crate::MessageKind as K;
    use crate::messages::{self as m, Direction::ToClient, Direction::ToDaemon};

    fn meta<M: WireCodec + crate::messages::Directed>(
        body: impl Body,
    ) -> Result<crate::messages::ArmMeta, CodecError> {
        decode::<M>(body).map(|msg| msg.meta())
    }
    Some(match (kind, direction) {
        (K::Conn, ToDaemon) => meta::<m::ConnToDaemonMsg>(body),
        (K::Conn, ToClient) => meta::<m::ConnToClientMsg>(body),
        (K::Input, ToDaemon) => meta::<m::InputMsg>(body),
        (K::Grid, ToClient) => meta::<m::GridMsg>(body),
        (K::Image, ToClient) => meta::<m::ImageMsg>(body),
        (K::Session, ToDaemon) => meta::<m::SessionToDaemonMsg>(body),
        (K::Session, ToClient) => meta::<m::SessionToClientMsg>(body),
        (K::Ops, ToDaemon) => meta::<m::OpsToDaemonMsg>(body),
        (K::Ops, ToClient) => meta::<m::OpsToClientMsg>(body),
        (K::Region, ToDaemon) => meta::<m::RegionToDaemonMsg>(body),
        (K::Region, ToClient) => meta::<m::RegionToClientMsg>(body),
        (K::Notify, ToDaemon) => meta::<m::NotifyToDaemonMsg>(body),
        (K::Notify, ToClient) => meta::<m::NotifyToClientMsg>(body),
        (K::Push, ToClient) => meta::<m::PushMsg>(body),
        (K::Search, ToDaemon) => meta::<m::SearchToDaemonMsg>(body),
        (K::Search, ToClient) => meta::<m::SearchToClientMsg>(body),
        (K::Input, ToClient) | (K::Grid | K::Image | K::Push, ToDaemon) => return None,
    })
}

/// The field numbers of the `direction` wrapper's arms of `kind`;
/// empty when no arm of `kind` travels that way.
#[must_use]
pub const fn arm_fields(
    kind: crate::MessageKind,
    direction: crate::messages::Direction,
) -> &'static [u32] {
    use crate::MessageKind as K;
    use crate::messages::{self as m, Direction::ToClient, Direction::ToDaemon};

    match (kind, direction) {
        (K::Conn, ToDaemon) => m::ConnToDaemonMsg::ARM_FIELDS,
        (K::Conn, ToClient) => m::ConnToClientMsg::ARM_FIELDS,
        (K::Input, ToDaemon) => m::InputMsg::ARM_FIELDS,
        (K::Grid, ToClient) => m::GridMsg::ARM_FIELDS,
        (K::Image, ToClient) => m::ImageMsg::ARM_FIELDS,
        (K::Session, ToDaemon) => m::SessionToDaemonMsg::ARM_FIELDS,
        (K::Session, ToClient) => m::SessionToClientMsg::ARM_FIELDS,
        (K::Ops, ToDaemon) => m::OpsToDaemonMsg::ARM_FIELDS,
        (K::Ops, ToClient) => m::OpsToClientMsg::ARM_FIELDS,
        (K::Region, ToDaemon) => m::RegionToDaemonMsg::ARM_FIELDS,
        (K::Region, ToClient) => m::RegionToClientMsg::ARM_FIELDS,
        (K::Notify, ToDaemon) => m::NotifyToDaemonMsg::ARM_FIELDS,
        (K::Notify, ToClient) => m::NotifyToClientMsg::ARM_FIELDS,
        (K::Push, ToClient) => m::PushMsg::ARM_FIELDS,
        (K::Search, ToDaemon) => m::SearchToDaemonMsg::ARM_FIELDS,
        (K::Search, ToClient) => m::SearchToClientMsg::ARM_FIELDS,
        (K::Input, ToClient) | (K::Grid | K::Image | K::Push, ToDaemon) => &[],
    }
}

/// The first top-level arm of `kind`'s `direction` wrapper in `body`,
/// found by walking field keys: the other wrapper decodes past it.
///
/// # Errors
/// [`CodecError::Prost`] on a malformed key or value.
pub fn arm_in(
    kind: crate::MessageKind,
    direction: crate::messages::Direction,
    mut body: &[u8],
) -> Result<Option<crate::messages::ArmMeta>, CodecError> {
    let fields = arm_fields(kind, direction);
    if fields.is_empty() {
        return Ok(None);
    }
    while !body.is_empty() {
        let key = read_varint(&mut body)?;
        let (field, wire_type) = (key >> 3, key & 7);
        if let Some(row) = fields.iter().position(|&arm| u64::from(arm) == field) {
            return Ok(kind.arms(direction).get(row).copied());
        }
        skip_value(&mut body, wire_type)?;
    }
    Ok(None)
}

/// One body of a family split by direction, read by a consumer that
/// does not know which way it traveled (a recording, a transcoder).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EitherHalf<D, C> {
    ToDaemon(D),
    ToClient(C),
}

/// Inverse of [`decode_either`].
#[must_use]
pub fn encode_either<D: WireCodec, C: WireCodec>(msg: &EitherHalf<D, C>) -> Vec<u8> {
    match msg {
        EitherHalf::ToDaemon(msg) => encode(msg),
        EitherHalf::ToClient(msg) => encode(msg),
    }
}

/// Decodes `body` as the half of a split family that names its arm.
///
/// # Errors
/// As [`decode`]; an arm of each half is
/// [`crate::convert::WireError::MixedDirections`].
pub fn decode_either<D, C>(body: &[u8]) -> Result<EitherHalf<D, C>, CodecError>
where
    D: WireCodec + crate::messages::Directed,
    C: WireCodec + crate::messages::Directed,
{
    use crate::messages::Direction;

    const {
        assert!(
            D::KIND as u16 == C::KIND as u16,
            "the two halves of one family"
        );
        assert!(matches!(D::DIRECTION, Direction::ToDaemon));
        assert!(matches!(C::DIRECTION, Direction::ToClient));
    }
    let to_daemon = arm_in(D::KIND, Direction::ToDaemon, body)?;
    let to_client = arm_in(D::KIND, Direction::ToClient, body)?;
    match (to_daemon, to_client) {
        (Some(to_daemon), Some(to_client)) => Err(crate::convert::WireError::MixedDirections {
            to_daemon: to_daemon.name,
            to_client: to_client.name,
        }
        .into()),
        (Some(_), None) => decode::<D>(body).map(EitherHalf::ToDaemon),
        (None, _) => decode::<C>(body).map(EitherHalf::ToClient),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each family's `WireCodec::KIND` matches the frame-kind table in
    /// `docs/reference/ipc.md`.
    #[test]
    fn wire_codec_kind_matches_the_documented_frame_kinds() {
        use crate::MessageKind as K;
        use crate::messages as m;
        assert_eq!(<m::ConnToDaemonMsg as WireCodec>::KIND, K::Conn);
        assert_eq!(<m::ConnToClientMsg as WireCodec>::KIND, K::Conn);
        assert_eq!(<m::InputMsg as WireCodec>::KIND, K::Input);
        assert_eq!(<m::GridMsg as WireCodec>::KIND, K::Grid);
        assert_eq!(<m::ImageMsg as WireCodec>::KIND, K::Image);
        assert_eq!(<m::SessionToDaemonMsg as WireCodec>::KIND, K::Session);
        assert_eq!(<m::SessionToClientMsg as WireCodec>::KIND, K::Session);
        assert_eq!(<m::OpsToDaemonMsg as WireCodec>::KIND, K::Ops);
        assert_eq!(<m::OpsToClientMsg as WireCodec>::KIND, K::Ops);
        assert_eq!(<m::RegionToDaemonMsg as WireCodec>::KIND, K::Region);
        assert_eq!(<m::RegionToClientMsg as WireCodec>::KIND, K::Region);
        assert_eq!(<m::NotifyToDaemonMsg as WireCodec>::KIND, K::Notify);
        assert_eq!(<m::NotifyToClientMsg as WireCodec>::KIND, K::Notify);
        assert_eq!(<m::PushMsg as WireCodec>::KIND, K::Push);
        assert_eq!(<m::SearchToDaemonMsg as WireCodec>::KIND, K::Search);
        assert_eq!(<m::SearchToClientMsg as WireCodec>::KIND, K::Search);
    }

    #[test]
    fn round_trips_a_domain_message() {
        let msg = crate::messages::ConnToClientMsg::Welcome { identity: None };
        let bytes = encode(&msg);
        let back: crate::messages::ConnToClientMsg = decode(&bytes).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn round_trips_a_u128_id() {
        let msg = crate::messages::PushMsg::Reattach {
            id: 0x0123_4567_89AB_CDEF_FEDC_BA98_7654_3210,
        };
        let bytes = encode(&msg);
        let back: crate::messages::PushMsg = decode(&bytes).unwrap();
        assert_eq!(back, msg);
    }

    /// What was stamped on is what a peer reads back, for both id kinds.
    #[test]
    fn a_correlated_encode_round_trips_its_envelope() {
        use crate::messages::{Correlation, RequestId, StreamId};

        let msg = crate::messages::OpsToDaemonMsg::List;
        let correlation = Correlation::request(RequestId::new(7).unwrap());
        let bytes = encode_correlated(&msg, correlation);
        assert_eq!(peek_correlation(&bytes).unwrap(), Some(correlation));
        assert_eq!(
            decode::<crate::messages::OpsToDaemonMsg>(&bytes).unwrap(),
            msg
        );

        let stream = Correlation::stream(StreamId::new(3).unwrap());
        let bytes = encode_correlated(
            &crate::messages::SearchToDaemonMsg::Query {
                query: "needle".into(),
                options: crate::messages::SearchOptions::default(),
            },
            stream,
        );
        assert_eq!(peek_correlation(&bytes).unwrap(), Some(stream));
    }

    /// The envelope costs the grid stream nothing, and an uncorrelated
    /// encode of a correlated family adds no bytes either.
    #[test]
    fn an_uncorrelated_encode_adds_no_bytes() {
        use crate::messages::{GridMsg, OpsToDaemonMsg};
        use crate::row::RowPayload;

        let delta = GridMsg::RowDelta {
            rows: vec![(0, RowPayload::from(vec![1u8; 64]))],
        };
        // Pinned as an absolute: a regression that stamped an envelope
        // onto grid frames would still round-trip.
        assert_eq!(encode(&delta).len(), 70);
        assert_eq!(peek_correlation(&encode(&delta)).unwrap(), None);

        assert_eq!(encode(&OpsToDaemonMsg::List).len(), 2);
    }

    /// An envelope naming neither id is refused: a sender that means
    /// "uncorrelated" omits the field.
    #[test]
    fn an_empty_envelope_is_refused() {
        use prost::Message as _;
        let wire = crate::wire::v1::OpsToDaemonMsg {
            msg: Some(crate::wire::v1::ops_to_daemon_msg::Msg::List(
                crate::wire::v1::OpsList {},
            )),
            correlation: Some(crate::wire::v1::Correlation { id: None }),
        };
        let err = peek_correlation(&wire.encode_to_vec()).unwrap_err();
        assert!(matches!(err, CodecError::Wire(_)), "got {err:?}");
    }

    /// The oneof's set arm still has to name a live id: 0 is how the
    /// wire spells "unset", so it is malformed rather than id zero.
    #[test]
    fn a_zero_id_in_the_set_arm_is_refused() {
        use prost::Message as _;
        for id in [
            crate::wire::v1::correlation::Id::RequestId(0),
            crate::wire::v1::correlation::Id::StreamId(0),
        ] {
            let wire = crate::wire::v1::OpsToDaemonMsg {
                msg: Some(crate::wire::v1::ops_to_daemon_msg::Msg::List(
                    crate::wire::v1::OpsList {},
                )),
                correlation: Some(crate::wire::v1::Correlation { id: Some(id) }),
            };
            let err = peek_correlation(&wire.encode_to_vec()).unwrap_err();
            assert!(matches!(err, CodecError::Wire(_)), "got {err:?}");
        }
    }

    fn push_varint(out: &mut Vec<u8>, mut value: u64) {
        loop {
            let byte = u8::try_from(value & 0x7f).unwrap();
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    /// A body carrying one field-100 occurrence per slice, each writing
    /// the `(tag, value)` pairs it is given in order.
    fn hand_built_body(slots: &[&[(u64, u64)]]) -> Vec<u8> {
        let mut body = Vec::new();
        for fields in slots {
            let mut slot = Vec::new();
            for &(tag, value) in *fields {
                push_varint(&mut slot, tag << 3);
                push_varint(&mut slot, value);
            }
            push_varint(&mut body, (100 << 3) | 2);
            push_varint(&mut body, u64::try_from(slot.len()).unwrap());
            body.extend_from_slice(&slot);
        }
        body
    }

    /// A body setting both tags is malformed, not "whichever prost saw
    /// last". Hand-built, because the generated type cannot write it.
    #[test]
    fn an_envelope_naming_both_ids_is_refused() {
        for slots in [
            &[&[(1, 1), (2, 2)][..]][..],
            &[&[(2, 2), (1, 1)][..]][..],
            &[&[(1, 7), (2, 1), (1, 0)][..]][..],
            &[&[(2, 1), (1, 0)][..]][..],
            &[&[(1, 0), (2, 1)][..]][..],
            &[&[(1, 7), (2, 0)][..]][..],
            &[&[(1, 7)][..], &[(2, 1)][..]][..],
        ] {
            let body = hand_built_body(slots);
            let err = peek_correlation(&body).unwrap_err();
            assert!(
                matches!(
                    err,
                    CodecError::Wire(crate::convert::WireError::AmbiguousCorrelation)
                ),
                "{slots:?} gave {err:?}"
            );
        }
    }

    /// One tag written twice is not two ids: the last value wins, as a
    /// canonical reader resolves it, and zero still spells "unset".
    #[test]
    fn one_tag_written_twice_keeps_its_last_value() {
        let body = hand_built_body(&[&[(1, 7), (1, 9)]]);
        assert_eq!(
            peek_correlation(&body).unwrap(),
            Some(crate::messages::Correlation::request(
                crate::messages::RequestId::new(9).unwrap()
            ))
        );
        let body = hand_built_body(&[&[(1, 7), (1, 0)]]);
        let err = peek_correlation(&body).unwrap_err();
        assert!(matches!(err, CodecError::Wire(_)), "got {err:?}");
    }

    /// An id tag encoded as anything but a varint is a decode error for
    /// a canonical reader, so felis must not read past it either.
    #[test]
    fn an_id_tag_that_is_not_a_varint_is_refused() {
        let mut slot = Vec::new();
        push_varint(&mut slot, (1 << 3) | 2);
        push_varint(&mut slot, 1);
        slot.push(7);
        let mut body = Vec::new();
        push_varint(&mut body, (100 << 3) | 2);
        push_varint(&mut body, u64::try_from(slot.len()).unwrap());
        body.extend_from_slice(&slot);
        let err = peek_correlation(&body).unwrap_err();
        assert!(matches!(err, CodecError::Prost(_)), "got {err:?}");
    }

    #[test]
    fn decode_rejects_a_malformed_body() {
        let err = decode::<crate::messages::ConnToDaemonMsg>(&[0xFF, 0xFF, 0xFF]).unwrap_err();
        assert!(matches!(err, CodecError::Prost(_)), "got {err:?}");
        let err = decode::<crate::messages::ConnToDaemonMsg>(&[]).unwrap_err();
        assert!(matches!(err, CodecError::Wire(_)), "got {err:?}");
    }

    fn joined(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    #[test]
    fn the_arm_walk_finds_an_arm_of_the_named_direction_before_or_after_the_other() {
        use crate::MessageKind;
        use crate::messages::{ConnToClientMsg, ConnToDaemonMsg, Direction};

        let hello = encode(&ConnToDaemonMsg::Hello {
            mode: crate::ConnectionMode::Ops,
            pull_paced: false,
        });
        let welcome = encode(&ConnToClientMsg::Welcome { identity: None });
        for body in [joined(&[&hello, &welcome]), joined(&[&welcome, &hello])] {
            let found = arm_in(MessageKind::Conn, Direction::ToClient, &body).unwrap();
            assert_eq!(found.map(|arm| arm.name), Some("Conn::Welcome"));
            let found = arm_in(MessageKind::Conn, Direction::ToDaemon, &body).unwrap();
            assert_eq!(found.map(|arm| arm.name), Some("Conn::Hello"));
        }
        assert_eq!(
            arm_in(MessageKind::Conn, Direction::ToClient, &hello).unwrap(),
            None
        );
    }

    #[test]
    fn the_arm_walk_refuses_a_malformed_key_or_value() {
        use crate::MessageKind;
        use crate::messages::Direction;

        let truncated_key: &[u8] = &[0x80];
        let overlong_varint: &[u8] = &[
            0x08, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01,
        ];
        let truncated_value: &[u8] = &[0x1A, 0x05, 0x00];
        let group: &[u8] = &[0x93, 0x03, 0x94, 0x03];
        for body in [truncated_key, overlong_varint, truncated_value, group] {
            assert!(
                matches!(
                    arm_in(MessageKind::Ops, Direction::ToClient, body),
                    Err(CodecError::Prost(_))
                ),
                "{body:02x?}"
            );
        }
    }

    /// A one-way family has no other wrapper to look for, so its bodies
    /// (the grid stream among them) are not walked at all.
    #[test]
    fn the_arm_walk_skips_a_family_with_no_arm_that_way() {
        use crate::MessageKind;
        use crate::messages::Direction;

        assert_eq!(
            arm_in(MessageKind::Grid, Direction::ToDaemon, &[0x80]).unwrap(),
            None
        );
    }

    #[test]
    fn a_body_naming_both_halves_reads_as_neither() {
        use crate::messages::{SessionToClientMsg, SessionToDaemonMsg};

        let detach = encode(&SessionToDaemonMsg::Detach);
        let accepted = encode(&SessionToClientMsg::InputAccepted);
        for body in [joined(&[&detach, &accepted]), joined(&[&accepted, &detach])] {
            assert!(
                matches!(
                    decode_either::<SessionToDaemonMsg, SessionToClientMsg>(&body),
                    Err(CodecError::Wire(
                        crate::convert::WireError::MixedDirections {
                            to_daemon: "Session::Detach",
                            to_client: "Session::InputAccepted",
                        }
                    ))
                ),
                "{body:02x?}"
            );
        }
    }

    /// The halves of a family number their arms apart, so a body reads
    /// as the half that sent it and a wrong-direction arm is never read
    /// as a neighbor of the other half.
    #[test]
    fn a_split_body_decodes_as_the_half_that_names_its_arm() {
        use crate::messages::{ConnToClientMsg, ConnToDaemonMsg, Direction};

        let cancel = ConnToDaemonMsg::Cancel {
            stream_id: crate::messages::StreamId::new(4).unwrap(),
        };
        let welcome = ConnToClientMsg::Welcome { identity: None };
        assert_eq!(
            decode_either::<ConnToDaemonMsg, ConnToClientMsg>(&encode(&cancel)).unwrap(),
            EitherHalf::ToDaemon(cancel.clone())
        );
        assert_eq!(
            decode_either::<ConnToDaemonMsg, ConnToClientMsg>(&encode(&welcome)).unwrap(),
            EitherHalf::ToClient(welcome.clone())
        );
        assert!(decode::<ConnToClientMsg>(&encode(&cancel)).is_err());
        assert!(decode::<ConnToDaemonMsg>(&encode(&welcome)).is_err());
        assert_eq!(
            arm_of(
                crate::MessageKind::Conn,
                Direction::ToClient,
                &encode(&welcome)
            )
            .unwrap()
            .unwrap()
            .name,
            "Conn::Welcome"
        );
        assert!(
            arm_of(
                crate::MessageKind::Grid,
                Direction::ToDaemon,
                &encode(&welcome)
            )
            .is_none()
        );
    }
}

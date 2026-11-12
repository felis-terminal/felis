//! The driver's contract (A-5/A-7), pinned case by case.

use felis_protocol::{
    ConnectionMode, ImageId, MessageKind, RowPayload,
    codec::{self, WireCodec},
    messages::{
        ConnToClientMsg, ConnToDaemonMsg, Correlation, GridMsg, ImageFormat, ImageMsg, ImageTarget,
        InputMsg, NotifyToClientMsg, NotifyToDaemonMsg, OpsToClientMsg, OpsToDaemonMsg, PushMsg,
        RegionSource, RegionToClientMsg, RegionToDaemonMsg, RequestId, SearchOptions,
        SearchToClientMsg, SearchToDaemonMsg, SessionToClientMsg, SessionToDaemonMsg,
        StreamErrorReason, StreamId, Subject,
    },
};

use super::*;

mod direction_and_malformed;
mod drained_frames;
mod envelope_class;
mod ids_and_streams;
mod phase_and_mode;
mod phase_ladder;
mod request_reply;

/// A driver welcomed and driven to `phase`. The transitions are the
/// driver's own, so a test cannot place it in a phase a connection
/// could not reach.
fn in_phase<R: Role>(phase: Phase, mode: ConnectionMode) -> ConnectionDriver<R> {
    let mut driver = ConnectionDriver::<R>::fresh(None);
    driver.preface_done();
    driver.handshake_done(mode);
    match phase {
        Phase::Setup => {}
        Phase::Attached => driver.attached(),
        Phase::Observing => driver.observing(),
        Phase::Preface | Phase::Handshake => {
            panic!("in_phase starts from a welcomed connection")
        }
    }
    driver
}

fn frame<M: WireCodec>(msg: &M) -> OwnedFrame {
    OwnedFrame {
        kind: M::KIND.as_u16(),
        body: Bytes::from(codec::encode(msg)),
    }
}

#[track_caller]
fn payload_of<R: Role>(driver: &mut ConnectionDriver<R>, f: &OwnedFrame) -> Payload {
    match driver.classify(f) {
        Ok(Incoming::Payload(p)) => p,
        other => panic!("expected a payload, got {other:?}"),
    }
}

#[track_caller]
fn decode_frame<M: WireCodec + Directed>(
    driver: &mut ConnectionDriver<impl Role>,
    f: &OwnedFrame,
) -> Result<Delivery<M>, DriverError> {
    let p = payload_of(driver, f);
    driver.decode::<M>(&p)
}

fn request(raw: u64) -> RequestId {
    RequestId::new(raw).expect("nonzero")
}

fn stream(raw: u64) -> StreamId {
    StreamId::new(raw).expect("nonzero")
}

fn a_query() -> SearchToDaemonMsg {
    SearchToDaemonMsg::Query {
        query: "needle".into(),
        options: SearchOptions::default(),
        source: felis_protocol::messages::SearchSource::Scrollback,
        session: None,
    }
}

fn a_match() -> SearchToClientMsg {
    SearchToClientMsg::Match {
        line_index: -1,
        text: "hit".into(),
        byte_spans: vec![],
        col_spans: vec![],
        anchor: None,
    }
}

fn open_on_daemon(driver: &mut DaemonDriver, id: StreamId) {
    let f = frame_correlated(&a_query(), Correlation::stream(id));
    assert!(matches!(
        decode_frame::<SearchToDaemonMsg>(driver, &f).unwrap(),
        Delivery::Deliver(_)
    ));
}

/// Field 100 stamped onto a body by hand: `slot` is the length-delimited
/// payload. The `Correlated` trait only exists on the families whose
/// wrapper declares the slot, and a peer that ignores that declaration
/// is exactly what this covers.
fn with_field_100<M: WireCodec>(msg: &M, slot: &[u8]) -> OwnedFrame {
    let mut body = codec::encode(msg);
    body.extend_from_slice(&[0xA2, 0x06]);
    body.push(u8::try_from(slot.len()).expect("a hand-written slot stays short"));
    body.extend_from_slice(slot);
    OwnedFrame {
        kind: M::KIND.as_u16(),
        body: Bytes::from(body),
    }
}

/// Field 100 holding a hand-written sequence of `(tag, value)` varint
/// pairs. Hand-written because the generated type has no way to write a
/// tag twice, which is what a peer naming both ids does.
fn with_hand_written_ids<M: WireCodec>(msg: &M, fields: &[(u8, u8)]) -> OwnedFrame {
    let slot: Vec<u8> = fields
        .iter()
        .flat_map(|&(tag, value)| [tag << 3, value])
        .collect();
    with_field_100(msg, &slot)
}

/// A `Correlation` naming request 1: tag 1 (`request_id`), value 1.
const REQUEST_ID_1: &[(u8, u8)] = &[(1, 1)];

/// A truncated varint, not a `Correlation`. The nested length is honest,
/// so prost skips field 100 as unknown and the family decode succeeds:
/// only the peek sees the fault, and only on a family whose wrapper
/// reserves the slot (one that declares it fails its own decode).
const MALFORMED_ENVELOPE: &[u8] = &[0xFF];

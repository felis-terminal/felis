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

fn correlated<M: Correlated>(msg: &M, correlation: Correlation) -> OwnedFrame {
    frame_correlated(msg, correlation)
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
    let f = correlated(&a_query(), Correlation::stream(id));
    let Incoming::Payload(p) = driver.classify(&f).unwrap() else {
        panic!("a search query is a payload");
    };
    assert!(matches!(
        driver.decode::<SearchToDaemonMsg>(&p).unwrap(),
        Delivery::Deliver(_)
    ));
}

/// Field 100 stamped onto a body by hand. The `Correlated` trait only
/// exists on the families whose wrapper declares the slot, and a peer
/// that ignores that declaration is exactly what this covers.
fn with_an_envelope<M: WireCodec>(msg: &M) -> OwnedFrame {
    let mut body = codec::encode(msg);
    // Tag 100, wire type 2, wrapping a `Correlation` whose request_id
    // is 1.
    body.extend_from_slice(&[0xA2, 0x06, 0x02, 0x08, 0x01]);
    OwnedFrame {
        kind: M::KIND.as_u16(),
        body: Bytes::from(body),
    }
}

/// Field 100 present but holding bytes that are not a `Correlation`.
/// The nested length is honest, so prost skips the whole field as an
/// unknown one and the family decode succeeds: only the peek sees the
/// fault, and only a family whose wrapper reserves the slot can reach
/// this state (one that declares it fails its own decode instead).
fn with_a_malformed_envelope<M: WireCodec>(msg: &M) -> OwnedFrame {
    let mut body = codec::encode(msg);
    // Tag 100, wire type 2, one byte of payload: 0xFF, a truncated
    // varint that is not a field key.
    body.extend_from_slice(&[0xA2, 0x06, 0x01, 0xFF]);
    OwnedFrame {
        kind: M::KIND.as_u16(),
        body: Bytes::from(body),
    }
}

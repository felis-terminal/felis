//! The driver never panics on arbitrary peer bytes: a security property,
//! since the daemon decodes attacker-reachable input here
//! (`docs/reference/testing.md` "Security tests").

#![allow(clippy::unwrap_used)]

use bytes::Bytes;
use felis_protocol::{
    ConnectionMode, MessageKind,
    codec::encode,
    messages::{
        ConnToDaemonMsg, Correlation, OpsToDaemonMsg, SearchOptions, SearchToDaemonMsg, StreamId,
    },
};
use felis_transport::{
    ConnectionDriver, Delivery, Incoming, OwnedFrame, StreamClass, driver::frame_correlated,
};
use proptest::prelude::*;

#[derive(Debug, Clone)]
enum Shaped {
    OpenStream(u64),
    Cancel(u64),
    BareList,
    Raw { kind: u16, body: Vec<u8> },
}

fn shaped() -> impl Strategy<Value = Shaped> {
    prop_oneof![
        (1u64..8).prop_map(Shaped::OpenStream),
        (0u64..8).prop_map(Shaped::Cancel),
        Just(Shaped::BareList),
        (0u16..12, prop::collection::vec(any::<u8>(), 0..24))
            .prop_map(|(kind, body)| Shaped::Raw { kind, body }),
    ]
}

fn render(shape: &Shaped) -> OwnedFrame {
    match shape {
        Shaped::OpenStream(id) => {
            let query = SearchToDaemonMsg::Query {
                query: "needle".into(),
                options: SearchOptions::default(),
            };
            // A generated 0 becomes the uncorrelated case.
            StreamId::new(*id).map_or_else(
                || OwnedFrame {
                    kind: MessageKind::Search.as_u16(),
                    body: Bytes::from(encode(&query)),
                },
                |id| frame_correlated(&query, Correlation::stream(id)),
            )
        }
        Shaped::Cancel(id) => {
            let msg = StreamId::new(*id).map_or(
                ConnToDaemonMsg::Hello {
                    mode: ConnectionMode::Ops,
                    pull_paced: false,
                },
                |stream_id| ConnToDaemonMsg::Cancel { stream_id },
            );
            OwnedFrame {
                kind: MessageKind::Conn.as_u16(),
                body: Bytes::from(encode(&msg)),
            }
        }
        Shaped::BareList => OwnedFrame {
            kind: MessageKind::Ops.as_u16(),
            body: Bytes::from(encode(&OpsToDaemonMsg::List)),
        },
        Shaped::Raw { kind, body } => OwnedFrame {
            kind: *kind,
            body: Bytes::from(body.clone()),
        },
    }
}

proptest! {
    /// A-7 allows exactly two outcomes per frame: progress or a typed
    /// close.
    #[test]
    fn an_arbitrary_frame_sequence_either_progresses_or_closes(
        shapes in prop::collection::vec(shaped(), 0..24)
    ) {
        let mut driver = ConnectionDriver::daemon();
        driver.preface_done();
        driver.handshake_done(ConnectionMode::Ops);

        for shape in &shapes {
            let frame = render(shape);
            let Ok(incoming) = driver.classify(&frame) else {
                return Ok(());
            };
            match incoming {
                Incoming::Control(_) | Incoming::CancelIgnored { .. } => {}
                Incoming::Payload(payload) => {
                    let decoded = match payload.kind {
                        MessageKind::Search => driver
                            .decode::<SearchToDaemonMsg>(&payload)
                            .map(|d| matches!(d, Delivery::Deliver(_))),
                        MessageKind::Ops => driver
                            .decode::<OpsToDaemonMsg>(&payload)
                            .map(|d| matches!(d, Delivery::Deliver(_))),
                        _ => Ok(true),
                    };
                    if decoded.is_err() {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// An id is never both active and terminated, and every id below the
    /// counter classifies.
    #[test]
    fn the_stream_table_stays_consistent(
        shapes in prop::collection::vec(shaped(), 0..24)
    ) {
        let mut driver = ConnectionDriver::daemon();
        driver.preface_done();
        driver.handshake_done(ConnectionMode::Ops);

        for shape in &shapes {
            let frame = render(shape);
            let Ok(incoming) = driver.classify(&frame) else { break };
            if let Incoming::Payload(payload) = incoming
                && payload.kind == MessageKind::Search
                && driver.decode::<SearchToDaemonMsg>(&payload).is_err()
            {
                break;
            }
            let live = driver.outstanding_streams();
            let counted = (1u64..32)
                .filter_map(StreamId::new)
                .filter(|id| {
                    matches!(
                        driver.classify_stream(*id),
                        StreamClass::Active | StreamClass::Canceled
                    )
                })
                .count();
            prop_assert_eq!(live, counted, "the live set and the classifier disagree");
        }
    }
}

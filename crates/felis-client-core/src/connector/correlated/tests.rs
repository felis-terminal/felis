#![cfg_attr(not(feature = "daemon-tests"), allow(unused))]

use std::time::Duration;

use felis_protocol::{
    ConnectionMode, MessageKind, codec,
    frame::{DEFAULT_MAX_BODY, Frame},
    messages::{
        ConnToClientMsg, Correlation, OpsToClientMsg, OpsToDaemonMsg, RefusalReason, RegionSource,
        RegionToClientMsg, RequestId, SearchOptions, SearchSource, SearchToClientMsg, SessionInfo,
        SpawnArgs, StreamId,
    },
};
use felis_transport::{FrameReader, FrameWriter, MAX_OUTSTANDING_STREAMS, TransportError};
use tokio::io::{DuplexStream, ReadHalf, WriteHalf};
use tokio::task::JoinHandle;

use super::{
    Closed, CorrelatedHandle, Family, ITEM_SLOT, MAX_OUTSTANDING_REQUESTS, MAX_QUEUED_BYTES,
    STREAM_BYTE_BUDGET, StreamEnd, StreamEvent, StreamReceiver,
};
use crate::connector::{ConnectError, Connection};

const PATIENCE: Duration = Duration::from_secs(10);
const BRIEFLY: Duration = Duration::from_millis(100);

type PeerRead = FrameReader<ReadHalf<DuplexStream>>;
type PeerWrite = FrameWriter<WriteHalf<DuplexStream>>;

/// A handle whose daemon is the returned frame pair, so the test writes
/// what no daemon would, over `buffer` bytes of socket each way.
fn scripted_over(buffer: usize, mode: ConnectionMode) -> (CorrelatedHandle, PeerRead, PeerWrite) {
    scripted_buffering(buffer, mode, 4)
}

fn scripted_buffering(
    buffer: usize,
    mode: ConnectionMode,
    stream_buffer: usize,
) -> (CorrelatedHandle, PeerRead, PeerWrite) {
    let (client, peer) = tokio::io::duplex(buffer);
    let (read, write) = tokio::io::split(client);
    let handle = Connection::from_halves(
        FrameReader::new(read),
        FrameWriter::at_build_minor(write),
        mode,
    )
    .into_correlated(stream_buffer)
    .unwrap();
    let (peer_read, peer_write) = tokio::io::split(peer);
    (
        handle,
        FrameReader::new(peer_read),
        FrameWriter::at_build_minor(peer_write),
    )
}

fn scripted() -> (CorrelatedHandle, PeerRead, PeerWrite) {
    scripted_over(64 * 1024, ConnectionMode::Ops)
}

fn request(id: u64) -> Correlation {
    Correlation::request(RequestId::new(id).unwrap())
}

fn listing(handle: &CorrelatedHandle) -> JoinHandle<Result<Vec<SessionInfo>, ConnectError>> {
    let handle = handle.clone();
    tokio::spawn(async move { handle.list_sessions().await })
}

async fn listed(
    list: JoinHandle<Result<Vec<SessionInfo>, ConnectError>>,
) -> Result<Vec<SessionInfo>, ConnectError> {
    tokio::time::timeout(PATIENCE, list).await.unwrap().unwrap()
}

/// An open search whose opener the peer has read.
async fn opened_search(
    handle: &CorrelatedHandle,
    peer_read: &mut PeerRead,
) -> StreamReceiver<SearchToClientMsg> {
    let search = handle
        .search(
            1,
            "x".to_owned(),
            SearchOptions::default(),
            SearchSource::Scrollback,
        )
        .await
        .unwrap();
    peer_read.next_frame().await.unwrap().expect("the opener");
    search
}

/// A list call and a search both waiting on the peer.
async fn a_call_and_a_stream_in_flight(
    handle: &CorrelatedHandle,
    peer_read: &mut PeerRead,
) -> (
    JoinHandle<Result<Vec<SessionInfo>, ConnectError>>,
    StreamReceiver<SearchToClientMsg>,
) {
    let search = opened_search(handle, peer_read).await;
    let list = listing(handle);
    peer_read.next_frame().await.unwrap().expect("the list");
    (list, search)
}

async fn drain<I: Family + std::fmt::Debug>(mut rx: StreamReceiver<I>) -> (Vec<I>, StreamEnd) {
    let mut items = Vec::new();
    loop {
        match tokio::time::timeout(PATIENCE, rx.next()).await {
            Ok(StreamEvent::Item(item)) => items.push(item),
            Ok(StreamEvent::End(end)) => return (items, end),
            Err(elapsed) => panic!("stream {} never ended: {elapsed}", rx.id()),
        }
    }
}

async fn eventually(mut done: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while !done() {
        assert!(tokio::time::Instant::now() < deadline, "never happened");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn a_match_of(bytes: usize) -> SearchToClientMsg {
    SearchToClientMsg::Match {
        line_index: 0,
        text: "x".repeat(bytes),
        byte_spans: Vec::new(),
        col_spans: Vec::new(),
        anchor: None,
    }
}

#[tokio::test]
async fn connection_end_terminates_every_open_request_and_stream() {
    let (handle, mut peer_read, peer_write) = scripted();
    let (list, search) = a_call_and_a_stream_in_flight(&handle, &mut peer_read).await;
    drop((peer_read, peer_write));

    let ended = |r| matches!(r, Err(ConnectError::ConnectionEnded(Closed::Eof)));
    assert!(ended(listed(list).await));
    assert_eq!(drain(search).await.1, StreamEnd::Closed(Closed::Eof));
    assert!(
        ended(handle.list_sessions().await),
        "a call after the end is refused unsent"
    );
}

#[tokio::test]
async fn an_uncorrelated_frame_ends_the_connection() {
    let (handle, mut peer_read, mut peer_write) = scripted();
    let (list, search) = a_call_and_a_stream_in_flight(&handle, &mut peer_read).await;
    let body = codec::encode(&OpsToClientMsg::Listed {
        sessions: Vec::new(),
    });
    peer_write
        .write_frame_unchecked(&Frame {
            kind: MessageKind::Ops.as_u16(),
            body: &body,
        })
        .await
        .unwrap();
    peer_write.flush().await.unwrap();

    let listed = listed(list).await;
    assert!(
        matches!(
            listed,
            Err(ConnectError::ConnectionEnded(Closed::Protocol(_)))
        ),
        "got {listed:?}"
    );
    assert!(matches!(
        drain(search).await.1,
        StreamEnd::Closed(Closed::Protocol(_))
    ));
    let after = tokio::time::timeout(PATIENCE, peer_read.next_frame())
        .await
        .expect("the client closes its side");
    assert!(
        matches!(after, Ok(None)),
        "nothing follows the violation: {after:?}"
    );
}

#[tokio::test]
async fn a_wrong_family_item_on_a_canceled_stream_ends_the_connection() {
    let (handle, mut peer_read, mut peer_write) = scripted();
    let search = opened_search(&handle, &mut peer_read).await;
    let id = search.id();
    drop(search);
    peer_read.next_frame().await.unwrap().expect("the cancel");

    peer_write
        .send_correlated(
            &RegionToClientMsg::RowsDone { exit_code: None },
            Correlation::stream(id),
        )
        .await
        .unwrap();
    eventually(|| handle.closed().is_some()).await;
    assert!(
        matches!(handle.closed(), Some(Closed::Protocol(_))),
        "got {:?}",
        handle.closed()
    );
}

#[tokio::test]
async fn a_refusal_ends_the_connection_with_the_daemons_reason() {
    let (handle, mut peer_read, mut peer_write) = scripted();
    let list = listing(&handle);
    peer_read.next_frame().await.unwrap().expect("the request");
    peer_write
        .send(&ConnToClientMsg::Refused {
            reason: RefusalReason::Role,
            detail: "not for this mode".to_owned(),
        })
        .await
        .unwrap();

    let listed = listed(list).await;
    assert!(
        matches!(
            listed,
            Err(ConnectError::ConnectionEnded(Closed::Refused {
                reason: RefusalReason::Role,
                ..
            }))
        ),
        "got {listed:?}"
    );
}

#[test]
fn a_daemon_at_capacity_is_worth_another_dial() {
    let at_capacity = Closed::Refused {
        reason: RefusalReason::AtCapacity,
        detail: String::new(),
    };
    assert!(at_capacity.is_transient());
    let ended = ConnectError::ConnectionEnded(at_capacity);
    assert!(ended.is_transient());
    assert_eq!(ended.at_capacity(), Some(""));
    assert!(
        !ended.may_be_a_cold_socket(),
        "a daemon answered, so nothing is cold"
    );
    assert!(
        !Closed::Refused {
            reason: RefusalReason::Role,
            detail: String::new(),
        }
        .is_transient()
    );
}

#[tokio::test]
async fn an_observer_takes_one_subscription_and_no_session_streams() {
    let (handle, _peer_read, _peer_write) = scripted_over(64 * 1024, ConnectionMode::Observer);

    assert!(matches!(
        handle.capture(1, RegionSource::Visible, false, None).await,
        Err(ConnectError::Driver(_))
    ));
    let _events = handle.subscribe_notifications(None).await.unwrap();
    assert!(matches!(
        handle.subscribe_notifications(None).await,
        Err(ConnectError::Driver(_))
    ));
}

#[tokio::test]
async fn an_unread_stream_lags_once_its_items_pass_the_byte_budget() {
    let (handle, mut peer_read, mut peer_write) = scripted();
    let search = opened_search(&handle, &mut peer_read).await;
    for _ in 0..3 {
        peer_write
            .send_correlated(
                &a_match_of(STREAM_BYTE_BUDGET * 2 / 5),
                Correlation::stream(search.id()),
            )
            .await
            .unwrap();
    }
    eventually(|| handle.open_streams() == 0).await;

    let (held, end) = drain(search).await;
    assert_eq!(
        (held.len(), end),
        (2, StreamEnd::Lagged),
        "two items fit the budget, the third would not"
    );
}

#[tokio::test]
async fn near_empty_items_pass_the_byte_budget_by_their_buffer_slots() {
    let (handle, mut peer_read, mut peer_write) =
        scripted_buffering(64 * 1024, ConnectionMode::Ops, usize::MAX);
    let search = opened_search(&handle, &mut peer_read).await;
    let tiny = a_match_of(0);
    let charge = codec::encoded_len_correlated(&tiny, Correlation::stream(search.id())) + ITEM_SLOT;
    let fit = STREAM_BYTE_BUDGET / charge;
    for _ in 0..=fit {
        peer_write
            .send_correlated(&tiny, Correlation::stream(search.id()))
            .await
            .unwrap();
    }
    eventually(|| handle.open_streams() == 0).await;

    let (held, end) = drain(search).await;
    assert_eq!((held.len(), end), (fit, StreamEnd::Lagged));
}

#[tokio::test]
async fn an_item_past_the_byte_budget_reaches_an_empty_buffer() {
    let (handle, mut peer_read, mut peer_write) = scripted();
    let search = opened_search(&handle, &mut peer_read).await;
    let large = a_match_of(STREAM_BYTE_BUDGET + 1);
    peer_write
        .send_correlated(&large, Correlation::stream(search.id()))
        .await
        .unwrap();
    peer_write
        .send(&ConnToClientMsg::End {
            stream_id: search.id(),
            count: 1,
        })
        .await
        .unwrap();

    assert_eq!(
        drain(search).await,
        (vec![large], StreamEnd::End { count: 1 })
    );
}

#[tokio::test]
async fn opening_and_dropping_streams_waits_once_the_cap_awaits_terminals() {
    let (handle, _peer_read, mut peer_write) = scripted();
    for _ in 0..MAX_OUTSTANDING_STREAMS {
        drop(
            handle
                .capture(1, RegionSource::Visible, false, None)
                .await
                .unwrap(),
        );
    }
    let next = handle.capture(1, RegionSource::Visible, false, None);
    tokio::pin!(next);
    assert!(
        tokio::time::timeout(BRIEFLY, &mut next).await.is_err(),
        "a dropped stream holds its place until the daemon's terminal"
    );

    peer_write
        .send(&ConnToClientMsg::End {
            stream_id: StreamId::new(1).unwrap(),
            count: 0,
        })
        .await
        .unwrap();
    let opened = tokio::time::timeout(PATIENCE, next)
        .await
        .expect("the terminal frees a place")
        .unwrap();
    assert_eq!(opened.id().get(), 33);
}

#[tokio::test]
async fn point_calls_wait_once_the_cap_is_unanswered() {
    let (handle, mut peer_read, mut peer_write) = scripted();
    let calls: Vec<_> = (0..=MAX_OUTSTANDING_REQUESTS)
        .map(|_| listing(&handle))
        .collect();
    for _ in 0..MAX_OUTSTANDING_REQUESTS {
        peer_read.next_frame().await.unwrap().expect("a request");
    }
    assert!(
        tokio::time::timeout(BRIEFLY, peer_read.next_frame())
            .await
            .is_err(),
        "nothing past the cap is written"
    );

    peer_write
        .send_correlated(
            &OpsToClientMsg::Listed {
                sessions: Vec::new(),
            },
            request(1),
        )
        .await
        .unwrap();
    tokio::time::timeout(PATIENCE, peer_read.next_frame())
        .await
        .expect("an answer frees a place")
        .unwrap()
        .expect("the waiting request");
    drop(calls);
}

#[tokio::test]
async fn queued_bytes_hold_a_call_until_the_writer_drains() {
    let (handle, mut peer_read, _peer_write) = scripted();
    let large = || {
        let handle = handle.clone();
        tokio::spawn(async move {
            let tag = "x".repeat(MAX_QUEUED_BYTES / 2 + 1);
            handle
                .set_tags("ab".to_owned(), vec![tag], Vec::new())
                .await
        })
    };
    let first = large();
    eventually(|| handle.issued_requests() == 1).await;
    let second = large();
    tokio::time::sleep(BRIEFLY).await;
    assert_eq!(
        handle.issued_requests(),
        1,
        "the second call waits for the first to be written"
    );

    let drained = tokio::spawn(async move {
        for _ in 0..2 {
            peer_read.next_frame().await.unwrap().expect("a call");
        }
    });
    eventually(|| handle.issued_requests() == 2).await;
    tokio::time::timeout(PATIENCE, drained)
        .await
        .unwrap()
        .unwrap();
    first.abort();
    second.abort();
}

#[tokio::test]
async fn a_call_past_the_frame_ceiling_is_refused_without_ending_the_connection() {
    let (handle, mut peer_read, mut peer_write) = scripted();
    let list = listing(&handle);
    peer_read.next_frame().await.unwrap().expect("the list");

    let oversized = handle
        .set_tags(
            "ab".to_owned(),
            vec!["x".repeat(DEFAULT_MAX_BODY as usize)],
            Vec::new(),
        )
        .await;
    assert!(
        matches!(
            oversized,
            Err(ConnectError::Transport(TransportError::Frame(_)))
        ),
        "got {oversized:?}"
    );

    peer_write
        .send_correlated(
            &OpsToClientMsg::Listed {
                sessions: Vec::new(),
            },
            request(1),
        )
        .await
        .unwrap();
    assert_eq!(listed(list).await.unwrap(), Vec::new());
    assert_eq!(handle.closed(), None);

    let next = listing(&handle);
    let frame = peer_read
        .next_frame()
        .await
        .unwrap()
        .expect("the next call");
    assert_eq!(
        codec::peek_correlation(&frame.body).unwrap(),
        Some(request(2)),
        "the refused call spent no id"
    );
    next.abort();
}

/// The last handle drops while a spawn is stuck mid-write to a peer
/// that never reads and a destroy waits behind it.
#[tokio::test]
async fn the_last_drop_stops_a_stalled_writer_and_sends_nothing_queued() {
    let (handle, mut peer_read, _peer_write) = scripted_over(16, ConnectionMode::Ops);
    let spawn = tokio::spawn({
        let handle = handle.clone();
        async move {
            let args = SpawnArgs {
                command: "/bin/sh".to_owned(),
                args: vec!["x".repeat(1024)],
                ..SpawnArgs::default()
            };
            handle.spawn_session(args).await
        }
    });
    tokio::time::sleep(BRIEFLY).await;
    let destroy = tokio::spawn({
        let handle = handle.clone();
        async move { handle.destroy_session("ab".to_owned()).await }
    });
    tokio::time::sleep(BRIEFLY).await;
    spawn.abort();
    destroy.abort();
    assert!(spawn.await.unwrap_err().is_cancelled());
    assert!(destroy.await.unwrap_err().is_cancelled());

    let tasks = handle.liveness();
    drop(handle);
    eventually(|| tasks.upgrade().is_none()).await;
    while let Ok(Some(frame)) = tokio::time::timeout(PATIENCE, peer_read.next_frame())
        .await
        .expect("the client's end closed")
    {
        let msg: OpsToDaemonMsg = codec::decode(&frame.body).unwrap();
        assert!(
            !matches!(msg, OpsToDaemonMsg::Destroy { .. }),
            "a destroy reached the daemon after its caller and the handle were gone"
        );
    }
}

#[cfg(all(unix, feature = "daemon-tests"))]
mod daemon {
    use super::*;
    use crate::connector::{DEFAULT_STREAM_BUFFER, Offer, connect, tests::daemon_running};

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_point_call_completes_past_a_stalled_stream_which_then_ends_lagged() {
        let (_tmp, path, server) = daemon_running("read _x").await;
        let handle = connect(&path, Offer::ops())
            .await
            .unwrap()
            .into_correlated(2)
            .unwrap();
        let session = handle.spawn_session(SpawnArgs::default()).await.unwrap();

        let stalled = handle
            .capture(session.id, RegionSource::Visible, false, None)
            .await
            .unwrap();
        eventually(|| handle.open_streams() == 0).await;
        let listed = tokio::time::timeout(PATIENCE, handle.list_sessions())
            .await
            .expect("the reply is not held behind the stalled stream")
            .unwrap();
        assert!(listed.iter().any(|s| s.id == session.id));

        let (rows, end) = drain(stalled).await;
        assert!(
            rows.iter()
                .all(|r| matches!(r, RegionToClientMsg::Row { .. }))
        );
        assert_eq!(
            (rows.len(), end),
            (2, StreamEnd::Lagged),
            "what the buffer held, then the lag"
        );

        let search = handle
            .search(
                session.id,
                "nothing-prints-this".to_owned(),
                SearchOptions::default(),
                SearchSource::Scrollback,
            )
            .await
            .unwrap();
        assert_eq!(
            drain(search).await,
            (Vec::new(), StreamEnd::End { count: 0 }),
            "the connection outlives the lagged stream's cancel"
        );
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn search_and_capture_on_two_sessions_share_one_connection() {
        let (_tmp, path, server) = daemon_running("read _x").await;
        let handle = connect(&path, Offer::ops())
            .await
            .unwrap()
            .into_correlated(DEFAULT_STREAM_BUFFER)
            .unwrap();
        let printing = |text: &str| SpawnArgs {
            command: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned(), format!("printf '{text}\\n'; read _x")],
            ..SpawnArgs::default()
        };
        let alpha = handle.spawn_session(printing("alpha-one")).await.unwrap();
        let bravo = handle.spawn_session(printing("bravo-two")).await.unwrap();

        for attempt in 0.. {
            assert!(attempt < 200, "the sessions never printed");
            let search = handle
                .search(
                    alpha.id,
                    "-".to_owned(),
                    SearchOptions::default(),
                    SearchSource::Scrollback,
                )
                .await
                .unwrap();
            let capture = handle
                .capture(bravo.id, RegionSource::Visible, false, None)
                .await
                .unwrap();
            let ((matches, searched), (rows, captured)) =
                tokio::join!(drain(search), drain(capture));

            let matched: Vec<_> = matches
                .iter()
                .map(|SearchToClientMsg::Match { text, .. }| text.as_str())
                .collect();
            let row_text: Vec<_> = rows
                .iter()
                .filter_map(|r| match r {
                    RegionToClientMsg::Row { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            assert!(
                matches!(searched, StreamEnd::End { .. }),
                "search: {searched:?}"
            );
            assert!(
                matches!(captured, StreamEnd::End { .. }),
                "capture: {captured:?}"
            );
            assert!(
                matched.iter().all(|t| !t.contains("bravo")),
                "the search read the other session: {matched:?}"
            );
            assert!(
                row_text.iter().all(|t| !t.contains("alpha")),
                "the capture read the other session: {row_text:?}"
            );
            if matched.iter().any(|t| t.contains("alpha-one"))
                && row_text.iter().any(|t| t.contains("bravo-two"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        server.abort();
    }
}

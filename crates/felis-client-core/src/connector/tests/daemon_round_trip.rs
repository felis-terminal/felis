use super::*;

use felis_protocol::messages::{SearchOptions, SearchToClientMsg, SearchToDaemonMsg};
use felis_transport::MAX_OUTSTANDING_STREAMS;

#[cfg(all(unix, feature = "daemon-tests"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connector_round_trips_through_a_real_daemon() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));

    let server_pool = pool.clone();
    let server = serve_bound(
        &path,
        DaemonCaps::default(),
        server_pool,
        default_session_factory(),
    );

    let mut conn = connect(&path, Offer::ops()).await.unwrap();
    assert_eq!(
        conn.effective_minor,
        felis_protocol::PROTOCOL_MINOR,
        "two same-build peers settle on their shared minor",
    );
    assert_eq!(
        conn.list_sessions().await.unwrap(),
        Vec::<SessionInfo>::new()
    );

    server.abort();
}

#[cfg(unix)]
async fn await_row_containing(
    conn: &mut Connection,
    target: &str,
    deadline: tokio::time::Instant,
) -> bool {
    while tokio::time::Instant::now() < deadline {
        let Ok(Ok(Some(frame))) =
            tokio::time::timeout(Duration::from_secs(3), conn.next_frame()).await
        else {
            return false;
        };
        if frame.kind != MessageKind::Grid.as_u16() {
            continue;
        }
        let msg: GridMsg = codec::decode(&frame.body).unwrap();
        if let GridMsg::RowDelta { rows } = msg {
            let mut styles = felis_grid::StyleTable::new();
            for (_, packed_cells) in &rows {
                let decoded = decode_row(&packed_cells.0, &mut styles).unwrap();
                let row_text: String = decoded
                    .cells
                    .iter()
                    .map(|c| match &c.grapheme {
                        felis_grid::Grapheme::Ascii(b) => *b as char,
                        felis_grid::Grapheme::Char(ch) => *ch,
                        // No cluster table here; the smoke tests search ASCII only.
                        felis_grid::Grapheme::Cluster(_) => '\u{FFFD}',
                        felis_grid::Grapheme::Empty
                        | felis_grid::Grapheme::Spacer
                        | felis_grid::Grapheme::SizedSpacer => ' ',
                    })
                    .collect();
                if row_text.contains(target) {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(unix)]
async fn collect_search(conn: &mut Connection, stream: StreamId) -> (usize, Result<u32, String>) {
    let mut items = 0usize;
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(10), conn.next_frame())
            .await
            .expect("search stream stalled")
            .expect("read")
            .expect("the stream must end with a terminal, not EOF");
        match conn
            .driver
            .classify(&frame)
            .expect("driver refused a frame")
        {
            Incoming::Control(ConnToClientMsg::End { stream_id, count }) => {
                assert_eq!(stream_id, stream, "terminal named another stream");
                return (items, Ok(count));
            }
            Incoming::Control(ConnToClientMsg::Error {
                subject: Subject::Stream(stream_id),
                detail,
                ..
            }) => {
                assert_eq!(stream_id, stream, "terminal named another stream");
                return (items, Err(detail));
            }
            Incoming::Payload(payload) if payload.kind == MessageKind::Search => {
                if let Delivery::Deliver(delivered) = conn
                    .driver
                    .decode::<SearchToClientMsg>(&payload)
                    .expect("decode")
                    && matches!(delivered.msg, SearchToClientMsg::Match { .. })
                {
                    items += 1;
                }
            }
            _ => {}
        }
    }
}

/// Every reply is matched to its own request while a search stream
/// runs on the same connection.
#[cfg(all(unix, feature = "daemon-tests"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_connection_answers_many_verbs_while_a_stream_runs() {
    // The `read` builtin parks the shell; `sleep` is not on the
    // sanitized PATH these tests run with.
    let (_tmp, path, server) = daemon_running("printf 'needle-alpha\\n'; read _x").await;

    let mut conn = connect(&path, Offer::ops()).await.unwrap();
    let attach = conn.create_with(SpawnArgs::default()).await.unwrap();
    let id = attach.id;
    // An `Ops` attach gets no grid burst, so the needle's arrival is
    // observed by retrying the search.
    let mut items = 0;
    let mut stream = conn.driver.open_stream().expect("the sequence is fresh");
    for attempt in 0..100 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            stream = conn.driver.open_stream().expect("the sequence is fresh");
        }
        conn.writer
            .send_correlated(
                &SearchToDaemonMsg::Query {
                    query: "needle-".to_owned(),
                    options: SearchOptions::default(),
                    source: felis_protocol::messages::SearchSource::Scrollback,
                    session: None,
                },
                Correlation::stream(stream),
            )
            .await
            .unwrap();
        let (found, outcome) = collect_search(&mut conn, stream).await;
        assert_eq!(outcome, Ok(u32::try_from(found).unwrap()));
        if found > 0 {
            items = found;
            break;
        }
    }
    assert!(items > 0, "the shell never printed the needle");

    let stream = conn.driver.open_stream().expect("the sequence is fresh");
    conn.writer
        .send_correlated(
            &SearchToDaemonMsg::Query {
                query: "needle-".to_owned(),
                options: SearchOptions::default(),
                source: felis_protocol::messages::SearchSource::Scrollback,
                session: None,
            },
            Correlation::stream(stream),
        )
        .await
        .unwrap();

    let listed = conn.list_sessions().await.expect("list on a live stream");
    assert!(
        listed.iter().any(|s| s.id == id),
        "the roster must list the session this connection attached"
    );
    let (resolved, tags, denied) = conn
        .set_tags(format!("{id:032x}"), vec!["marked".to_owned()], Vec::new())
        .await
        .expect("tag on a live stream");
    assert_eq!(resolved, ResolvedId::Ok { id });
    assert_eq!(tags, vec!["marked".to_owned()]);
    assert!(denied.is_none());
    let switched = conn
        .switch_session(
            format!("{id:032x}"),
            format!("{id:032x}"),
            SwitchScope::Default,
        )
        .await
        .expect("switch on a live stream");
    assert_eq!(switched.from, ResolvedId::Ok { id });
    assert_eq!(switched.to, Some(ResolvedId::Ok { id }));
    assert_eq!(switched.queued, 0);
    assert_eq!(switched.denied, None);
    let listed = conn.list_sessions().await.expect("second list");
    assert!(
        listed
            .iter()
            .any(|s| s.id == id && s.tags == vec!["marked".to_owned()]),
        "the second list must observe the tag the earlier verb set",
    );

    let (found, outcome) = collect_search(&mut conn, stream).await;
    assert!(found > 0, "the search must have found the needle");
    assert_eq!(outcome, Ok(u32::try_from(found).unwrap()));

    server.abort();
}

/// The daemon's slot is freed by the terminal its outbound pump
/// writes, which no in-process driver test can reach.
#[cfg(all(unix, feature = "daemon-tests"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sequential_streams_free_their_slot_on_the_daemon() {
    let (_tmp, path, server) = daemon_running("read _x").await;

    let mut conn = connect(&path, Offer::ops()).await.unwrap();
    drop(conn.create_with(SpawnArgs::default()).await.unwrap());

    for round in 0..MAX_OUTSTANDING_STREAMS + 8 {
        let stream = conn.driver.open_stream().expect("the sequence is fresh");
        conn.writer
            .send_correlated(
                &SearchToDaemonMsg::Query {
                    query: "needle-".to_owned(),
                    options: SearchOptions::default(),
                    source: felis_protocol::messages::SearchSource::Scrollback,
                    session: None,
                },
                Correlation::stream(stream),
            )
            .await
            .unwrap();
        let (_found, outcome) = collect_search(&mut conn, stream).await;
        assert!(
            outcome.is_ok(),
            "search #{round} was refused: {outcome:?} — the daemon never freed its slot",
        );
        assert_eq!(conn.driver.outstanding_streams(), 0, "round {round}");
    }

    server.abort();
}

#[cfg(all(unix, feature = "daemon-tests"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_window_is_refused_a_mutating_verb_without_losing_the_connection() {
    let (_tmp, path, server) = daemon_running("read _x").await;

    let mut conn = connect(&path, Offer::window(false)).await.unwrap();
    let attach = conn.create_with(SpawnArgs::default()).await.unwrap();
    let id = attach.id;

    let refused = conn
        .set_tags(format!("{id:032x}"), vec!["nope".to_owned()], Vec::new())
        .await;
    assert!(
        matches!(
            refused,
            Err(ConnectError::StreamRefused {
                reason: StreamErrorReason::InvalidRequest,
                ..
            })
        ),
        "a window's Tag must be refused typed, got {refused:?}",
    );

    let listed = conn.list_sessions().await.expect("list after a refusal");
    assert!(listed.iter().any(|s| s.id == id));

    server.abort();
}

#[cfg(all(unix, feature = "daemon-tests"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_a_search_mid_stream_stops_it_and_leaves_the_connection_usable() {
    // Deep enough that the walk spans dozens of slices for the cancel to land between.
    let (_tmp, path, server) =
        daemon_running("i=0; while [ $i -lt 9000 ]; do echo needle-$i; i=$((i+1)); done; read _x")
            .await;

    let mut conn = connect(&path, Offer::window(false)).await.unwrap();
    conn.create_with(SpawnArgs::default()).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    assert!(
        await_row_containing(&mut conn, "needle-8999", deadline).await,
        "the shell never filled the scrollback"
    );

    let query = || SearchToDaemonMsg::Query {
        query: "needle-".to_owned(),
        options: SearchOptions::default(),
        source: felis_protocol::messages::SearchSource::Scrollback,
        session: None,
    };

    let full = conn.driver.open_stream().expect("the sequence is fresh");
    conn.writer
        .send_correlated(&query(), Correlation::stream(full))
        .await
        .unwrap();
    let (_found, outcome) = collect_search(&mut conn, full).await;
    let full_count = outcome.expect("an uncanceled walk ends clean");

    // The cancel races the walk across the socket, so one attempt may
    // arrive after the last slice; a cancel that never stops the walk
    // loses every attempt.
    let mut shipped = Vec::new();
    for _ in 0..10 {
        let stream = conn.driver.open_stream().expect("the sequence is fresh");
        conn.writer
            .send_correlated(&query(), Correlation::stream(stream))
            .await
            .unwrap();
        conn.driver.cancel_stream(stream);
        conn.writer
            .send(&ConnToDaemonMsg::Cancel { stream_id: stream })
            .await
            .unwrap();
        let (_dropped, outcome) = collect_search(&mut conn, stream).await;
        let count = outcome.expect("a canceled stream ends clean");
        assert_eq!(
            conn.driver.classify_stream(stream),
            felis_transport::StreamClass::Terminated,
        );
        shipped.push(count);
        if count < full_count {
            break;
        }
    }
    assert!(
        shipped.iter().any(|&count| count < full_count),
        "no cancel stopped the walk early: shipped {shipped:?} of {full_count} matches",
    );

    let listed = conn.list_sessions().await.expect("list after a cancel");
    assert_eq!(listed.len(), 1, "the session is still there");

    server.abort();
}

#[cfg(all(unix, feature = "daemon-tests"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_huge_scrollback_search_emits_before_the_walk_completes() {
    let (_tmp, path, server) =
        daemon_running("i=0; while [ $i -lt 2000 ]; do echo needle-$i; i=$((i+1)); done; read _x")
            .await;

    let mut conn = connect(&path, Offer::window(false)).await.unwrap();
    conn.create_with(SpawnArgs::default()).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    assert!(
        await_row_containing(&mut conn, "needle-1999", deadline).await,
        "the shell never filled the scrollback"
    );

    let stream = conn.driver.open_stream().expect("the sequence is fresh");
    conn.writer
        .send_correlated(
            &SearchToDaemonMsg::Query {
                query: "needle-".to_owned(),
                options: SearchOptions::default(),
                source: felis_protocol::messages::SearchSource::Scrollback,
                session: None,
            },
            Correlation::stream(stream),
        )
        .await
        .unwrap();

    loop {
        let frame = tokio::time::timeout(Duration::from_secs(15), conn.next_frame())
            .await
            .expect("no first match within the deadline")
            .expect("read")
            .expect("eof before the first match");
        match conn
            .driver
            .classify(&frame)
            .expect("driver refused a frame")
        {
            Incoming::Control(ConnToClientMsg::End { .. } | ConnToClientMsg::Error { .. }) => {
                panic!("the walk finished before shipping a single item");
            }
            Incoming::Payload(payload) if payload.kind == MessageKind::Search => {
                let Delivery::Deliver(delivered) = conn
                    .driver
                    .decode::<SearchToClientMsg>(&payload)
                    .expect("decode")
                else {
                    continue;
                };
                if matches!(delivered.msg, SearchToClientMsg::Match { .. }) {
                    break;
                }
            }
            _ => {}
        }
    }
    assert_eq!(
        conn.driver.classify_stream(stream),
        felis_transport::StreamClass::Active,
        "the stream must still be walking when its first item lands",
    );

    server.abort();
}

/// Both halves are `Window` connections: only the window burst
/// carries grid rows.
#[cfg(all(unix, feature = "daemon-tests"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killed_client_reattaches_and_observes_prior_output() {
    let (_tmp, path, pool, server) = daemon_with_pool("printf reattach-ok; read _x");

    let mut a = connect(&path, Offer::window(false)).await.unwrap();
    let attach = a.create_with(SpawnArgs::default()).await.unwrap();
    let session_id = attach.id;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    assert!(
        await_row_containing(&mut a, "reattach-ok", deadline).await,
        "client A never saw the sentinel"
    );
    // No explicit Detach: the close-mid-loop path a hung-up window takes.
    drop(a);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while pool
        .lock()
        .await
        .get(felis_daemon::SessionId(session_id))
        .is_some_and(|row| row.meta.lock().unwrap().subscribers > 0)
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the daemon never saw the window go"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let mut b = connect(&path, Offer::window(false)).await.unwrap();
    assert!(
        b.list_sessions()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session_id),
        "Ops::List did not report the surviving session"
    );
    b.attach(session_id, AttachIntent::Deliberate)
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    assert!(
        await_row_containing(&mut b, "reattach-ok", deadline).await,
        "client B's rehydration did not contain the prior shell output"
    );

    server.abort();
}

#[cfg(all(unix, feature = "daemon-tests"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn destroy_session_removes_session_and_acks_existed() {
    let (_tmp, path, server) = daemon_running("read _x").await;

    let mut a = connect(&path, Offer::ops()).await.unwrap();
    let id = a.create_with(SpawnArgs::default()).await.unwrap().id;
    drop(a);

    let mut killer = connect(&path, Offer::ops()).await.unwrap();
    let resolved = killer
        .destroy_session(felis_protocol::SessionHex(id).to_string())
        .await
        .unwrap();
    assert_eq!(
        resolved,
        ResolvedId::Ok { id },
        "session must have been found and removed"
    );
    drop(killer);

    let mut probe = connect(&path, Offer::ops()).await.unwrap();
    assert!(
        !probe
            .list_sessions()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == id),
        "destroyed session must not reappear in the roster",
    );

    server.abort();
}

#[cfg(all(unix, feature = "daemon-tests"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn destroy_session_on_missing_id_returns_existed_false() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let server_pool = pool.clone();
    let server = serve_bound(
        &path,
        DaemonCaps::default(),
        server_pool,
        default_session_factory(),
    );

    let mut conn = connect(&path, Offer::ops()).await.unwrap();
    let resolved = conn.destroy_session("deadbeef".to_owned()).await.unwrap();
    assert_eq!(
        resolved,
        ResolvedId::NoMatch,
        "destroy on a non-existent prefix must resolve NoMatch, not error",
    );

    server.abort();
}

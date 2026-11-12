use super::*;

/// A search emits in bounded slices: the first slice reaches the
/// subscriber while the walk still has scrollback left.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_search_ships_its_first_slice_before_the_walk_completes() {
    let mut task = SessionTask::for_tests(session_with(
        "i=0; while [ $i -lt 400 ]; do echo needle-$i; i=$((i+1)); done; read _x",
    ));
    let (sub, mut rx) = attach_sub(&mut task, false);
    wait_for_grid_text(&task.session.core, "needle-399").await;
    drain_events(&mut rx);

    let stream = test_stream(1);
    task.handle_search(
        Owner::Sub(sub),
        SearchToDaemonMsg::Query {
            query: "needle-".to_owned(),
            options: felis_protocol::messages::SearchOptions::default(),
            source: felis_protocol::messages::SearchSource::Scrollback,
            session: None,
        },
        stream,
    );
    assert!(
        task.has_pending_chunks(),
        "the walk must be queued, not run"
    );
    task.run_next_chunk();

    let first = drain_events(&mut rx);
    assert!(
        first.iter().any(|ev| matches!(ev, OutEvent::Search { .. })),
        "the first slice must carry hits, got {} events",
        first.len(),
    );
    assert!(
        !first.iter().any(|ev| matches!(
            ev,
            OutEvent::Control(ConnToClientMsg::End { .. } | ConnToClientMsg::Error { .. })
        )),
        "a walk this deep cannot finish in one slice",
    );
    assert!(
        task.has_pending_chunks(),
        "the unfinished walk must re-queue itself",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_between_slices_stops_the_walk_with_one_terminal() {
    let mut task = SessionTask::for_tests(session_with(
        "i=0; while [ $i -lt 400 ]; do echo needle-$i; i=$((i+1)); done; read _x",
    ));
    let (sub, mut rx) = attach_sub(&mut task, false);
    wait_for_grid_text(&task.session.core, "needle-399").await;
    drain_events(&mut rx);

    let stream = test_stream(1);
    task.handle_search(
        Owner::Sub(sub),
        SearchToDaemonMsg::Query {
            query: "needle-".to_owned(),
            options: felis_protocol::messages::SearchOptions::default(),
            source: felis_protocol::messages::SearchSource::Scrollback,
            session: None,
        },
        stream,
    );
    task.run_next_chunk();
    drain_events(&mut rx);

    task.cancel_producer(Owner::Sub(sub), stream);
    let mut turns = 0;
    while task.has_pending_chunks() {
        task.run_next_chunk();
        turns += 1;
        assert!(turns < 8, "a canceled producer must stop promptly");
    }
    let after = drain_events(&mut rx);
    assert!(
        !after.iter().any(|ev| matches!(ev, OutEvent::Search { .. })),
        "no hit may be emitted after the cancel",
    );
    let terminals = after
        .iter()
        .filter(|ev| {
            matches!(
                ev,
                OutEvent::Control(ConnToClientMsg::End { .. } | ConnToClientMsg::Error { .. })
            )
        })
        .count();
    assert_eq!(terminals, 1, "exactly one terminal, even under cancel");
}

/// The query's source reaches the walk: the same needle on the
/// alternate screen is missing from the default search and found by
/// the visible one, at its live row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_visible_search_walks_the_alternate_screen() {
    use felis_protocol::messages::{SearchOptions, SearchSource};

    let mut task = SessionTask::for_tests(session_with(
        "printf '\\033[?1049h\\033[2;1Hon-alt'; read _x",
    ));
    let (sub, mut rx) = attach_sub(&mut task, false);
    wait_for_grid_text(&task.session.core, "on-alt").await;
    drain_events(&mut rx);

    let mut hits = |source, id| {
        task.handle_search(
            Owner::Sub(sub),
            SearchToDaemonMsg::Query {
                query: "on-alt".to_owned(),
                options: SearchOptions::default(),
                source,
                session: None,
            },
            test_stream(id),
        );
        while task.has_pending_chunks() {
            task.run_next_chunk();
        }
        drain_events(&mut rx)
            .into_iter()
            .filter_map(|ev| match ev {
                OutEvent::Search {
                    msg: SearchToClientMsg::Match { line_index, .. },
                    ..
                } => Some(line_index),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(hits(SearchSource::Scrollback, 1), Vec::<i64>::new());
    assert_eq!(hits(SearchSource::Visible, 2), vec![1]);
}

/// A visible walk with more hits than one send carries resumes at
/// the row after the last hit sent: every row once, in order, under
/// one terminal counting them all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_visible_search_resumes_past_the_item_cap_at_the_next_row() {
    use felis_protocol::messages::{SearchOptions, SearchSource};

    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (sub, mut rx) = attach_sub(&mut task, false);
    drain_events(&mut rx);
    let rows: u16 = 300;
    {
        let mut core = task.session.lock_core();
        let ParseCore { parser, grid, .. } = &mut *core;
        *grid = felis_grid::Grid::new(rows, 8);
        parser.advance(grid, "hit\r\n".repeat(usize::from(rows) - 1).as_bytes());
        parser.advance(grid, b"hit");
    }

    task.handle_search(
        Owner::Sub(sub),
        SearchToDaemonMsg::Query {
            query: "hit".to_owned(),
            options: SearchOptions::default(),
            source: SearchSource::Visible,
            session: None,
        },
        test_stream(1),
    );
    let mut sends = Vec::new();
    let mut reported = Vec::new();
    let mut terminal = None;
    while task.has_pending_chunks() {
        task.run_next_chunk();
        let mut items = 0;
        for ev in drain_events(&mut rx) {
            match ev {
                OutEvent::Search {
                    msg: SearchToClientMsg::Match { line_index, .. },
                    ..
                } => {
                    items += 1;
                    reported.push(line_index);
                }
                OutEvent::Control(ConnToClientMsg::End { count, .. }) => terminal = Some(count),
                _ => {}
            }
        }
        sends.push(items);
    }
    let cap = usize::try_from(SEARCH_CHUNK_ITEMS).unwrap();
    assert_eq!(sends, vec![cap, cap, usize::from(rows) - 2 * cap]);
    assert_eq!(reported, (0..i64::from(rows)).collect::<Vec<_>>());
    assert_eq!(terminal, Some(u32::from(rows)));
}

/// An unresolvable needle ends the stream with a typed error, not
/// an empty success.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_needle_ends_the_stream_with_a_typed_error() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (sub, mut rx) = attach_sub(&mut task, false);
    drain_events(&mut rx);

    let stream = test_stream(1);
    task.handle_search(
        Owner::Sub(sub),
        SearchToDaemonMsg::Query {
            query: String::new(),
            options: felis_protocol::messages::SearchOptions::default(),
            source: felis_protocol::messages::SearchSource::Scrollback,
            session: None,
        },
        stream,
    );
    let events = drain_events(&mut rx);
    assert!(
        events.iter().any(|ev| matches!(
            ev,
            OutEvent::Control(ConnToClientMsg::Error {
                subject: Subject::Stream(id),
                reason: StreamErrorReason::InvalidRequest,
                ..
            }) if *id == stream
        )),
        "an empty needle must terminate the stream typed, got {events:?}",
    );
    assert!(
        !task.has_pending_chunks(),
        "a refused query must queue no walk",
    );
}

/// A row stream's terminal follows `RowsDone` rather than replacing
/// it, and its count is the rows emitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_stream_ends_with_rows_done_then_its_terminal() {
    let mut task = SessionTask::for_tests(session_with("printf 'CAPTURE\\n'; read _x"));
    let (sub, mut rx) = attach_sub(&mut task, false);
    wait_for_grid_text(&task.session.core, "CAPTURE").await;
    drain_events(&mut rx);

    let stream = test_stream(1);
    task.start_rows(Owner::Sub(sub), RegionSource::Visible, false, None, stream);
    while task.has_pending_chunks() {
        task.run_next_chunk();
    }
    let events = drain_events(&mut rx);
    let done_at = events.iter().position(|ev| {
        matches!(
            ev,
            OutEvent::Region {
                msg: RegionToClientMsg::RowsDone { .. },
                ..
            }
        )
    });
    let end_at = events
        .iter()
        .position(|ev| matches!(ev, OutEvent::Control(ConnToClientMsg::End { .. })));
    assert!(
        matches!((done_at, end_at), (Some(d), Some(e)) if d < e),
        "RowsDone must precede the stream terminal, got {events:?}",
    );

    let rows = events
        .iter()
        .filter(|ev| {
            matches!(
                ev,
                OutEvent::Region {
                    msg: RegionToClientMsg::Row { .. },
                    ..
                }
            )
        })
        .count();
    let count = events
        .iter()
        .find_map(|ev| match ev {
            OutEvent::Control(ConnToClientMsg::End { count, .. }) => Some(*count),
            _ => None,
        })
        .expect("the stream terminal");
    assert_eq!(
        usize::try_from(count).expect("a count fits usize"),
        rows,
        "the terminal's count is the rows emitted, got {events:?}",
    );
}

/// A region that resolves to nothing still ends with a terminal, at
/// `count == 0`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_region_stream_ends_with_a_zero_count() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (sub, mut rx) = attach_sub(&mut task, false);
    drain_events(&mut rx);

    let stream = test_stream(1);
    task.start_rows(
        Owner::Sub(sub),
        RegionSource::LastCommand,
        false,
        None,
        stream,
    );
    while task.has_pending_chunks() {
        task.run_next_chunk();
    }
    let events = drain_events(&mut rx);
    let count = events
        .iter()
        .find_map(|ev| match ev {
            OutEvent::Control(ConnToClientMsg::End { count, .. }) => Some(*count),
            _ => None,
        })
        .expect("the stream terminal");
    assert_eq!(
        count, 0,
        "an unresolved region emits no rows, got {events:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eviction_terminates_a_subscribers_open_streams() {
    let mut task = SessionTask::for_tests(session_with(
        "i=0; while [ $i -lt 400 ]; do echo needle-$i; i=$((i+1)); done; read _x",
    ));
    let (sub, mut rx) = attach_sub(&mut task, false);
    wait_for_grid_text(&task.session.core, "needle-399").await;
    drain_events(&mut rx);

    let stream = test_stream(1);
    task.handle_search(
        Owner::Sub(sub),
        SearchToDaemonMsg::Query {
            query: "needle-".to_owned(),
            options: felis_protocol::messages::SearchOptions::default(),
            source: felis_protocol::messages::SearchSource::Scrollback,
            session: None,
        },
        stream,
    );
    task.run_next_chunk();
    drain_events(&mut rx);

    task.evict_all(Some("forced"));
    let events = drain_events(&mut rx);
    assert!(
        events.iter().any(|ev| matches!(
            ev,
            OutEvent::Control(ConnToClientMsg::Error {
                subject: Subject::Stream(id),
                reason: StreamErrorReason::Unavailable,
                ..
            }) if *id == stream
        )),
        "the open stream must be failed typed on eviction, got {events:?}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_push_evicting_a_window_terminates_its_open_streams() {
    let mut task = SessionTask::for_tests(session_with(
        "i=0; while [ $i -lt 400 ]; do echo needle-$i; i=$((i+1)); done; read _x",
    ));
    let (sub, mut rx) = attach_sub(&mut task, false);
    wait_for_grid_text(&task.session.core, "needle-399").await;
    drain_events(&mut rx);

    let stream = test_stream(1);
    task.handle_search(
        Owner::Sub(sub),
        SearchToDaemonMsg::Query {
            query: "needle-".to_owned(),
            options: felis_protocol::messages::SearchOptions::default(),
            source: felis_protocol::messages::SearchSource::Scrollback,
            session: None,
        },
        stream,
    );
    task.run_next_chunk();
    drain_events(&mut rx);

    let idx = task.sub_index(sub).expect("subscriber present");
    task.subs[idx].buffered.store(
        SUBSCRIBER_BUFFER_CAP - WIRE_LEN_OVERHEAD + 1,
        Ordering::Relaxed,
    );

    let accepted = task.push_to_scope(&PushMsg::Reattach { id: task.id.0 }, SwitchScope::Default);
    assert_eq!(queued(accepted), 0, "an over-cap window accepts nothing");
    assert!(
        task.sub_index(sub).is_none(),
        "an over-cap window departs on the push",
    );
    let events = drain_events(&mut rx);
    assert!(
        events.iter().any(|ev| matches!(
            ev,
            OutEvent::Control(ConnToClientMsg::Error {
                subject: Subject::Stream(id),
                reason: StreamErrorReason::Unavailable,
                ..
            }) if *id == stream
        )),
        "the open stream must be failed typed when a push evicts, got {events:?}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn region_reply_reaches_the_originator_only() {
    let mut task = SessionTask::for_tests(session_with("printf 'CLIPME'; read _x"));
    let (sub_a, mut rx_a) = attach_sub(&mut task, false);
    let (_sub_b, mut rx_b) = attach_sub(&mut task, false);
    wait_for_grid_text(&task.session.core, "CLIPME").await;

    task.handle_region(
        Owner::Sub(sub_a),
        RegionSource::Visible,
        false,
        test_request(),
    );

    let a = drain_events(&mut rx_a);
    let data = a
        .iter()
        .find_map(|ev| match ev {
            OutEvent::Region {
                msg: RegionToClientMsg::Reply { data, .. },
                ..
            } => Some(data.clone()),
            _ => None,
        })
        .expect("originating subscriber must receive a region reply");
    assert!(
        String::from_utf8_lossy(&data).contains("CLIPME"),
        "the reply must carry the serialized region, got {:?}",
        String::from_utf8_lossy(&data),
    );
    assert!(
        !drain_events(&mut rx_b).iter().any(|ev| matches!(
            ev,
            OutEvent::Region {
                msg: RegionToClientMsg::Reply { .. },
                ..
            }
        )),
        "a region read is the originator's action, not broadcast",
    );
}

/// One self-delimiting token: a scalar, an escape sequence, or a
/// newline. The generator knows every safe cut offset without
/// walking the bytes, which is what makes it an oracle for
/// `resumable_cut`.
fn arb_token() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        Just(b"x".to_vec()),
        Just(b"\n".to_vec()),
        Just("\u{3042}".as_bytes().to_vec()),
        Just("\u{1F600}".as_bytes().to_vec()),
        Just(b"\x1b[31m".to_vec()),
        Just(b"\x1b]0;t\x07".to_vec()),
        Just(b"\x1bP+q436f\x1b\\".to_vec()),
        Just(b"\x1bM".to_vec()),
    ]
}

proptest! {
    /// An oversize region ships its youngest lines: the cut is the
    /// first line start at or after the byte bound, and where the
    /// tail holds no newline (a blob, or one long `--ansi` line) it
    /// is still a token boundary, never mid-sequence or mid-scalar.
    #[test]
    fn an_oversize_region_reply_cuts_at_the_first_safe_offset(
        tokens in prop::collection::vec(arb_token(), 1..24),
        cap_ratio in 0.0f64..1.0,
    ) {
        let mut data = Vec::new();
        let mut starts = vec![0usize];
        for t in &tokens {
            data.extend_from_slice(t);
            starts.push(data.len());
        }
        let position = Some(RegionPosition {
            top_line: 1,
            cursor_line: 2,
            cursor_column: 3,
        });

        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "ratio of a bounded length")]
        let cap = (data.len() as f64 * cap_ratio) as usize;
        let (out, out_position) = trim_region_reply(data.clone(), position, cap);
        if data.len() <= cap {
            prop_assert_eq!(out, data);
            prop_assert_eq!(out_position, position);
            return Ok(());
        }

        prop_assert_eq!(out_position, None, "a trimmed reply drops its head-relative position");
        prop_assert!(data.ends_with(&out), "the reply keeps a suffix");
        prop_assert!(out.len() <= cap, "the suffix fits the bound");

        let tail = data.len() - cap;
        let cut = data.len() - out.len();
        // The end of the buffer is not a cut: it would answer empty.
        let safe: Vec<usize> = starts
            .iter()
            .copied()
            .filter(|&b| b >= tail && b < data.len())
            .collect();
        let want = safe
            .iter()
            .copied()
            .find(|&b| b > 0 && data[b - 1] == b'\n')
            .or_else(|| safe.first().copied())
            .unwrap_or(data.len());
        prop_assert_eq!(cut, want);
    }
}

/// A region the daemon cannot resolve (no `OSC 133` marks yet) still
/// answers, with empty bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unresolvable_region_still_answers_with_empty_bytes() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (sub, mut rx) = attach_sub(&mut task, false);
    drain_events(&mut rx);

    task.handle_region(
        Owner::Sub(sub),
        RegionSource::LastCommand,
        false,
        test_request(),
    );

    let reply = drain_events(&mut rx)
        .into_iter()
        .find_map(|ev| match ev {
            OutEvent::Region {
                msg: msg @ RegionToClientMsg::Reply { .. },
                ..
            } => Some(msg),
            _ => None,
        })
        .expect("an unresolvable region must still produce a reply");
    assert_eq!(
        reply,
        RegionToClientMsg::Reply {
            data: Vec::new(),
            position: None,
            exit_code: None,
        },
    );
}

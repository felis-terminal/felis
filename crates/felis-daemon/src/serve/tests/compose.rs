use super::*;

/// One `Attention` per cycle however many BELs arrived.
#[test]
fn collect_facets_coalesces_bel_bytes_into_one_bell() {
    let mut grid = Grid::new(2, 8);
    let mut parser = felis_vt::Parser::new();
    parser.advance(&mut grid, b"\x07\x07\x07");

    let bells = collect_facets(&mut grid)
        .into_iter()
        .filter(|m| {
            matches!(
                m,
                GridMsg::Attention {
                    source: AttentionSource::Bell
                }
            )
        })
        .count();
    assert_eq!(
        bells, 1,
        "exactly one Bell per cycle even with multiple BEL bytes",
    );

    assert!(
        collect_facets(&mut grid)
            .iter()
            .all(|m| !matches!(m, GridMsg::Attention { .. })),
        "Bell drained on cycle 1 — must not re-fire on quiescent cycle 2",
    );
}

/// Pins the fan-out contract in docs/reference/protocols/notifications.md.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fan_out_publishes_decoded_notification_to_subscribers() {
    use felis_protocol::messages::Urgency;

    let pool = SessionPool::new();
    let mut rx = pool.subscribe_notifications();
    let hub = pool.notify_hub();

    let mut grid = Grid::new(2, 20);
    let mut parser = felis_vt::Parser::new();
    parser.advance(&mut grid, b"\x1b]99;u=2;Build failed\x1b\\");

    let id = SessionId(0xABCD);
    let latest = take_notifications(&mut grid)
        .and_then(|taken| publish_notifications(&hub, id, taken, false))
        .expect("a decoded notification must publish and be returned for the meta snapshot");
    assert_eq!(latest.title.as_deref(), Some("Build failed"));
    assert_eq!(latest.urgency, Urgency::Critical);

    let ev = tokio::time::timeout(Duration::from_millis(200), rx.recv())
        .await
        .expect("recv did not time out")
        .expect("subscriber received an event");
    match ev {
        NotifyToClientMsg::Event {
            session_id,
            notification,
            attached,
            ..
        } => {
            assert_eq!(session_id, 0xABCD);
            assert_eq!(notification.title.as_deref(), Some("Build failed"));
            assert_eq!(notification.body, "");
            assert_eq!(notification.urgency, Urgency::Critical);
            assert!(!attached, "the parked drain path sets attached=false");
        }
        other => panic!("expected NotifyToClientMsg::Event, got {other:?}"),
    }

    assert!(take_notifications(&mut grid).is_none());
}

/// `ModeFlags` ships only on drift against the last value shipped to this
/// subscriber.
#[test]
fn compose_diffs_emits_mode_flags_on_drift_only() {
    let mut grid = Grid::new(2, 8);
    let mut parser = felis_vt::Parser::new();
    // modifyOtherKeys must survive the diff path so the encoder can
    // disambiguate Shift+Enter; DECSCNM reaches pixels only via this
    // snapshot.
    parser.advance(&mut grid, b"\x1b[?2004h\x1b[?1049h\x1b[>4;2m\x1b[?5h");

    let mut stream = seeded_stream(&grid);
    stream.diff.last_mode_flags = felis_grid::ModeSnapshot::default();
    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");

    let payload = grid_msgs(&out)
        .into_iter()
        .find(|m| matches!(m, GridMsg::ModeFlags { .. }))
        .expect("ModeFlags emitted on drift");
    let GridMsg::ModeFlags {
        bracketed_paste,
        alt_screen,
        mouse_protocol,
        application_cursor,
        modify_other_keys,
        application_keypad,
        win32_input_mode,
        reverse_video,
    } = payload
    else {
        unreachable!()
    };
    assert!(bracketed_paste, "?2004h flipped bracketed_paste on");
    assert!(alt_screen, "?1049h flipped alt_screen on");
    assert_eq!(mouse_protocol, MouseProtocol::Off, "no mouse mode set");
    assert!(!application_cursor, "DECCKM not toggled in this fixture");
    assert_eq!(
        modify_other_keys,
        ModifyOtherKeys::Level2,
        "CSI > 4 ; 2 m reached the wire"
    );
    assert!(!application_keypad, "DECKPAM not toggled in this fixture");
    assert!(!win32_input_mode, "?9001 not toggled in this fixture");
    assert!(reverse_video, "?5h reached the wire");

    let mut out2 = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out2).expect("compose ok");
    assert!(
        grid_msgs(&out2)
            .iter()
            .all(|m| !matches!(m, GridMsg::ModeFlags { .. })),
        "ModeFlags must not re-fire when nothing changed",
    );
}

/// DECSCNM lives only in the mode snapshot (no row byte carries it), so a
/// rehydrate must ship it.
#[test]
fn rehydrate_ships_the_active_reverse_video_state() {
    let mut grid = Grid::new(2, 8);
    felis_vt::Parser::new().advance(&mut grid, b"\x1b[?5hhello");

    let images = felis_grid::images::ImageStore::new(1024);
    let placements = felis_grid::images::Placements::new();
    let mut out = Vec::new();
    SubscriberStream::rehydrated(
        ConnectionMode::Window,
        &grid,
        &images,
        &placements,
        &mut out,
    )
    .expect("rehydrate compose");

    let flags = grid_msgs(&out)
        .into_iter()
        .find(|m| matches!(m, GridMsg::ModeFlags { .. }))
        .expect("rehydrate ships a ModeFlags snapshot");
    assert!(
        matches!(
            flags,
            GridMsg::ModeFlags {
                reverse_video: true,
                ..
            }
        ),
        "attaching to a reversed session must arrive reversed",
    );
}

/// A cell names a palette index, never a color, so `OSC 4`/`OSC 104` reach
/// the client only as palette facets.
#[test]
fn collect_facets_ships_palette_sets_and_the_whole_table_reset() {
    let mut grid = Grid::new(2, 8);
    let mut parser = felis_vt::Parser::new();
    parser.advance(&mut grid, b"\x1b]4;1;#ff0000;2;#00ff00\x1b\\");
    assert_eq!(
        collect_facets(&mut grid)
            .into_iter()
            .filter(|m| matches!(m, GridMsg::PaletteColor { .. } | GridMsg::PaletteResetAll))
            .collect::<Vec<_>>(),
        vec![
            GridMsg::PaletteColor {
                index: 1,
                action: PaletteAction::Set {
                    rgb: (0xFF, 0x00, 0x00),
                },
            },
            GridMsg::PaletteColor {
                index: 2,
                action: PaletteAction::Set {
                    rgb: (0x00, 0xFF, 0x00),
                },
            },
        ],
    );

    parser.advance(&mut grid, b"\x1b]104;1\x1b\\");
    assert!(collect_facets(&mut grid).contains(&GridMsg::PaletteColor {
        index: 1,
        action: PaletteAction::Reset,
    }));
    parser.advance(&mut grid, b"\x1b]104\x1b\\");
    assert!(collect_facets(&mut grid).contains(&GridMsg::PaletteResetAll));

    assert!(
        collect_facets(&mut grid).is_empty(),
        "palette facets must not re-fire when nothing changed",
    );
}

/// A window paints the overrides it was sent until told otherwise, so
/// a RIS that drops them must say so.
#[test]
fn collect_facets_ships_the_color_and_pointer_resets_a_ris_implies() {
    let mut grid = Grid::new(2, 8);
    let mut parser = felis_vt::Parser::new();
    parser.advance(
        &mut grid,
        b"\x1b]4;1;#ff0000\x1b\\\x1b]11;#102030\x1b\\\x1b]22;text\x1b\\",
    );
    drop(collect_facets(&mut grid));
    parser.advance(&mut grid, b"\x1bc");
    let facets = collect_facets(&mut grid);
    assert!(facets.contains(&GridMsg::PaletteResetAll), "{facets:?}");
    assert!(
        facets.contains(&GridMsg::ThemeColor {
            channel: ThemeChannel::Background,
            action: ThemeAction::Reset,
        }),
        "{facets:?}"
    );
    assert!(
        facets.contains(&GridMsg::PointerShape { name: None }),
        "{facets:?}"
    );
}

/// The burst has no other carrier for a palette entry.
#[test]
fn rehydrate_replays_the_palette_override_layer_ascending() {
    let mut grid = Grid::new(2, 8);
    felis_vt::Parser::new().advance(
        &mut grid,
        b"\x1b]4;200;#010203;7;#040506;33;#070809\x1b\\\x1b]104;33\x1b\\",
    );

    let images = felis_grid::images::ImageStore::new(1024);
    let placements = felis_grid::images::Placements::new();
    let mut out = Vec::new();
    SubscriberStream::rehydrated(
        ConnectionMode::Window,
        &grid,
        &images,
        &placements,
        &mut out,
    )
    .expect("rehydrate compose");

    assert_eq!(
        grid_msgs(&out)
            .into_iter()
            .filter(|m| matches!(m, GridMsg::PaletteColor { .. } | GridMsg::PaletteResetAll))
            .collect::<Vec<_>>(),
        vec![
            GridMsg::PaletteColor {
                index: 7,
                action: PaletteAction::Set {
                    rgb: (0x04, 0x05, 0x06),
                },
            },
            GridMsg::PaletteColor {
                index: 200,
                action: PaletteAction::Set {
                    rgb: (0x01, 0x02, 0x03),
                },
            },
        ],
    );
}

/// Registry entries must precede any `RowDelta` that references them: the
/// shadow resolves ids against tables already applied.
#[test]
fn compose_diffs_ships_new_hyperlinks_and_clusters_before_row_deltas() {
    let mut grid = Grid::new(1, 8);
    let mut parser = felis_vt::Parser::new();
    let mut stream = seeded_stream(&grid);
    parser.advance(
        &mut grid,
        b"\x1b]8;;https://example.com\x07a\x1b]8;;\x07e\xcc\x81",
    );
    merge_damage(&mut grid, &mut stream);

    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");

    let msgs = grid_msgs(&out);
    let position = |pred: fn(&GridMsg) -> bool, what: &str| {
        msgs.iter()
            .position(pred)
            .unwrap_or_else(|| panic!("no {what} in {msgs:?}"))
    };
    let link = position(|m| matches!(m, GridMsg::Hyperlink { .. }), "Hyperlink");
    let cluster = position(|m| matches!(m, GridMsg::Cluster { .. }), "Cluster");
    let row = position(|m| matches!(m, GridMsg::RowDelta { .. }), "RowDelta");
    assert!(
        link < row,
        "Hyperlink at {link} must precede RowDelta at {row}"
    );
    assert!(
        cluster < row,
        "Cluster at {cluster} must precede RowDelta at {row}",
    );
}

fn grid_with_scrolled_out_clusters() -> Grid {
    let mut grid = Grid::new(2, 8);
    felis_vt::Parser::new().advance(
        &mut grid,
        "a\u{0301}\r\nb\u{0301}\r\nc\u{0301}\r\nd\u{0301}".as_bytes(),
    );
    assert_eq!(grid.cluster_count(), 4, "one interned cluster per line");
    assert!(
        !grid.scrollback().is_empty(),
        "the first lines scrolled off"
    );
    grid
}

fn cluster_ids(out: &[OutEvent]) -> Vec<u32> {
    grid_msgs(out)
        .into_iter()
        .filter_map(|msg| match msg {
            GridMsg::Cluster { id, .. } => Some(id),
            _ => None,
        })
        .collect()
}

fn row_delta_at(out: &[OutEvent]) -> usize {
    grid_msgs(out)
        .iter()
        .position(|m| matches!(m, GridMsg::RowDelta { .. }))
        .expect("a composed burst carries a RowDelta")
}

/// The attach burst carries only the registry entries its rows name.
#[test]
fn rehydrate_ships_only_the_clusters_its_visible_rows_reference() {
    let grid = grid_with_scrolled_out_clusters();
    let mut out = Vec::new();
    let _stream = seeded_stream_into(&grid, &mut out);

    assert_eq!(
        cluster_ids(&out),
        vec![3, 4],
        "the scrollback-only clusters 1 and 2 stay out of the burst",
    );
    let last_cluster = grid_msgs(&out)
        .iter()
        .rposition(|m| matches!(m, GridMsg::Cluster { .. }))
        .expect("the visible clusters ship");
    assert!(
        last_cluster < row_delta_at(&out),
        "every entry a row references still precedes that row",
    );
}

/// Later compose cycles drain the unreferenced tail, so the client's table
/// converges on the daemon's.
#[test]
fn the_registry_tail_the_burst_skipped_backfills_on_later_cycles() {
    let mut grid = grid_with_scrolled_out_clusters();
    let mut burst = Vec::new();
    let mut stream = seeded_stream_into(&grid, &mut burst);

    let mut out = Vec::new();
    dirty_a_row(&mut grid, &mut stream);
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");
    assert_eq!(
        cluster_ids(&out),
        vec![1, 2],
        "the scrollback-only entries follow, lowest id first",
    );

    let mut quiet = Vec::new();
    dirty_a_row(&mut grid, &mut stream);
    compose_one(&mut grid, &mut stream, &mut quiet).expect("compose ok");
    assert!(
        cluster_ids(&quiet).is_empty(),
        "a caught-up subscriber gets no repeat",
    );
}

fn dirty_a_row(grid: &mut Grid, stream: &mut SubscriberStream) {
    felis_vt::Parser::new().advance(grid, b"x");
    merge_damage(grid, stream);
}

/// The drain rides cycles that already carry content: a frame asks for a
/// repaint, so a drain on quiescent cycles would repaint an unchanged
/// screen once per 256 entries.
#[test]
fn the_registry_drain_does_not_manufacture_a_frame_on_a_quiet_cycle() {
    let mut grid = grid_with_scrolled_out_clusters();
    let mut burst = Vec::new();
    let mut stream = seeded_stream_into(&grid, &mut burst);
    assert_eq!(cluster_ids(&burst), vec![3, 4], "burst precondition");

    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");
    assert!(
        out.is_empty(),
        "nothing changed on screen, so the cycle emits nothing: {out:?}",
    );

    dirty_a_row(&mut grid, &mut stream);
    let mut busy = Vec::new();
    compose_one(&mut grid, &mut stream, &mut busy).expect("compose ok");
    assert_eq!(cluster_ids(&busy), vec![1, 2]);
}

/// An `Ops` connection decodes no row payload, so it gets neither rows nor
/// registry entries; shipping them would be dropped traffic plus a per-cell
/// handle walk over every dirty row forever.
#[test]
fn an_ops_subscriber_is_sent_neither_rows_nor_registry_entries() {
    let mut grid = grid_with_scrolled_out_clusters();
    let images = felis_grid::images::ImageStore::new(1024);
    let placements = felis_grid::images::Placements::new();
    let mut burst = Vec::new();
    let mut stream =
        SubscriberStream::rehydrated(ConnectionMode::Ops, &grid, &images, &placements, &mut burst)
            .expect("rehydrate compose");
    assert!(
        cluster_ids(&burst).is_empty(),
        "no entries in the ops burst"
    );

    felis_vt::Parser::new().advance(&mut grid, "e\u{0301}".as_bytes());
    merge_damage(&mut grid, &mut stream);
    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");
    assert!(
        grid_msgs(&out).iter().all(|m| !matches!(
            m,
            GridMsg::Cluster { .. } | GridMsg::Hyperlink { .. } | GridMsg::RowDelta { .. }
        )),
        "an ops subscriber receives no rows, no referenced entries, and no drain: {out:?}",
    );
}

/// Browsing back composes rows naming ids the visible-first burst skipped;
/// a tail cursor would report them sent, so the sent-set must know they are
/// holes.
#[test]
fn browsing_into_scrollback_ships_the_entries_those_rows_name_first() {
    let mut grid = grid_with_scrolled_out_clusters();
    let mut burst = Vec::new();
    let mut stream = seeded_stream_into(&grid, &mut burst);
    assert_eq!(cluster_ids(&burst), vec![3, 4], "burst precondition");

    stream.diff.viewport = 1;
    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");

    let msgs = grid_msgs(&out);
    let entry = msgs
        .iter()
        .position(|m| matches!(m, GridMsg::Cluster { id: 2, .. }))
        .expect("the scrollback row's cluster must ship for the composed view");
    assert!(
        entry < row_delta_at(&out),
        "Cluster at {entry} must precede the composed RowDelta at {}",
        row_delta_at(&out),
    );
}

fn prompt_mark_count(out: &[OutEvent]) -> usize {
    grid_msgs(out)
        .iter()
        .filter(|m| matches!(m, GridMsg::PromptMark { .. }))
        .count()
}

/// A window's shadow drops prompt marks and its prompt jump resolves
/// daemon-side, so neither its burst nor its live cycles carry them.
#[test]
fn a_window_is_sent_no_prompt_marks_in_its_burst_or_its_cycles() {
    let mut grid = Grid::new(2, 8);
    let mut parser = felis_vt::Parser::new();
    parser.advance(&mut grid, b"\x1b]133;A\x07$ \x1b]133;B\x07");
    let mut burst = Vec::new();
    let mut stream = seeded_stream_into(&grid, &mut burst);
    assert_eq!(prompt_mark_count(&burst), 0, "burst: {burst:?}");

    parser.advance(&mut grid, b"\x1b]133;C\x07\x1b]133;D;0\x07");
    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");
    assert_eq!(prompt_mark_count(&out), 0, "cycle: {out:?}");
}

/// The cursor counts absolute ordinals minus `prompt_marks_pruned`, so a
/// front-prune cannot skip a mark the subscriber never received.
#[test]
fn compose_diffs_streams_marks_across_a_front_prune_without_skipping() {
    let mut grid = Grid::new(2, 8);
    let mut parser = felis_vt::Parser::new();
    parser.advance(&mut grid, b"\x1b[1;1H\x1b]133;A\x07");
    let mut stream = ops_stream_into(&grid, &mut Vec::new());
    assert_eq!(stream.diff.prompt_marks_sent, 1);

    for _ in 0..felis_grid::DEFAULT_SCROLLBACK_ROWS + 5 {
        parser.advance(&mut grid, b"\r\n");
    }
    assert_eq!(grid.prompt_marks(), []);
    assert_eq!(grid.prompt_marks_pruned(), 1);

    parser.advance(&mut grid, b"\x1b]133;A\x07");
    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");
    assert_eq!(
        prompt_mark_count(&out),
        1,
        "the post-prune mark ships once, not skipped"
    );
    let mut out2 = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out2).expect("compose ok");
    assert_eq!(
        prompt_mark_count(&out2),
        0,
        "no mark re-fires on a quiescent cycle"
    );
}

/// Composition lives on the daemon: the shadow has no scrollback ring, only
/// a mirror of shipped `RowDelta`s.
#[test]
fn compose_diffs_ships_composed_view_on_viewport_flip_into_browse() {
    let mut grid = Grid::new(2, 8);
    let mut parser = felis_vt::Parser::new();
    parser.advance(&mut grid, b"row1\r\nrow2\r\nrow3\r\nrow4\r\nrow5");
    assert!(!grid.scrollback().is_empty());

    let mut stream = seeded_stream(&grid);
    stream.diff.viewport = 1;
    merge_damage(&mut grid, &mut stream);

    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");

    let msgs = grid_msgs(&out);
    let row_entry_count: usize = msgs
        .iter()
        .filter_map(|m| match m {
            GridMsg::RowDelta { rows } => Some(rows.len()),
            _ => None,
        })
        .sum();
    let viewport_state_count = msgs
        .iter()
        .filter(|m| matches!(m, GridMsg::ViewportState { .. }))
        .count();
    let hidden_cursor_count = msgs
        .iter()
        .filter(|m| matches!(m, GridMsg::CursorState { visible: false, .. }))
        .count();
    assert_eq!(
        row_entry_count,
        usize::from(grid.rows()),
        "viewport flip must cover every visible row in the composed RowDelta",
    );
    assert_eq!(
        viewport_state_count, 1,
        "first cycle past drift must emit ViewportState exactly once",
    );
    assert_eq!(
        hidden_cursor_count, 1,
        "cursor must hide on browse entry so the live cursor doesn't render mid-scrollback",
    );
}

/// The continuation bit rides every live-row `RowDelta` so the shadow can
/// stitch logical lines for triple-click selection.
#[test]
fn compose_diffs_carries_the_soft_wrap_bit_on_live_rows() {
    let mut grid = Grid::new(2, 3);
    felis_vt::Parser::new().advance(&mut grid, b"abcdef");
    assert!(grid.row_soft_wrap_continued(1), "fixture must autowrap");

    let mut stream = seeded_stream(&grid);
    merge_damage(&mut grid, &mut stream);
    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");

    let mut bits: Vec<(u16, bool)> = Vec::new();
    for msg in grid_msgs(&out) {
        if let GridMsg::RowDelta { rows } = msg {
            for (row, packed_cells) in rows {
                let decoded =
                    felis_grid::decode_row(&packed_cells.0, &mut felis_grid::StyleTable::new())
                        .unwrap();
                bits.push((row, decoded.soft_wrap_continued));
            }
        }
    }
    bits.sort_unstable();
    assert_eq!(bits, vec![(0, false), (1, true)]);
}

/// The per-cycle row cache is a shared encode: what it hands the second
/// subscriber must be byte-identical to encoding the row again.
#[test]
fn mirrored_subscribers_share_byte_identical_row_encodings() {
    let mut grid = Grid::new(2, 8);
    let mut first = seeded_stream(&grid);
    let mut second = seeded_stream(&grid);
    let mut uncached = seeded_stream(&grid);
    felis_vt::Parser::new().advance(&mut grid, b"hi\r\nthere");
    for stream in [&mut first, &mut second, &mut uncached] {
        stream.damage.merge(grid.damage());
    }
    grid.damage_mut().clear();

    let mut cache = RowEncodeCache::default();
    cache.begin_cycle(2);
    let mut out_first = Vec::new();
    let mut out_second = Vec::new();
    let mut out_uncached = Vec::new();
    compose_diffs(&mut grid, &mut first, &mut cache, &mut out_first).expect("compose ok");
    compose_diffs(&mut grid, &mut second, &mut cache, &mut out_second).expect("compose ok");
    compose_one(&mut grid, &mut uncached, &mut out_uncached).expect("compose ok");

    let row_deltas = |out: &[OutEvent]| -> Vec<(u16, Vec<u8>)> {
        let mut rows: Vec<(u16, Vec<u8>)> = grid_msgs(out)
            .into_iter()
            .filter_map(|msg| match msg {
                GridMsg::RowDelta { rows } => Some(rows),
                _ => None,
            })
            .flatten()
            .map(|(row, packed_cells)| (row, packed_cells.0))
            .collect();
        rows.sort_unstable();
        rows
    };
    let first_rows = row_deltas(&out_first);
    assert_eq!(first_rows.len(), 2, "both dirty rows ship");
    assert_eq!(first_rows, row_deltas(&out_second), "cache hit differs");
    assert_eq!(first_rows, row_deltas(&out_uncached), "cache miss differs");
}

/// Rows above the scrollback seam read the ring's wrapped side-band, rows
/// below it the live grid's bit.
#[test]
fn browse_compose_sources_the_wrap_bit_across_the_scrollback_seam() {
    let mut grid = Grid::new(2, 3);
    let mut parser = felis_vt::Parser::new();
    parser.advance(&mut grid, b"abcdef\r\nxx\r\nyy");
    assert_eq!(grid.scrollback().len(), 2);

    let mut stream = seeded_stream(&grid);
    stream.diff.viewport = 1;
    merge_damage(&mut grid, &mut stream);
    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");

    for msg in grid_msgs(&out) {
        if let GridMsg::RowDelta { rows } = msg {
            for (row, packed_cells) in rows {
                let decoded =
                    felis_grid::decode_row(&packed_cells.0, &mut felis_grid::StyleTable::new())
                        .unwrap();
                assert_eq!(
                    decoded.soft_wrap_continued,
                    row == 0,
                    "visible row {row} ({:?})",
                    row_text(&packed_cells.0),
                );
            }
        }
    }
}

/// Ring rows index negatively (`-1` the youngest), live rows by grid
/// coordinate: the stream `felis sessions capture --source scrollback`
/// prints.
#[test]
fn region_rows_window_scrollback_walks_ring_then_screen() {
    let mut grid = Grid::new(2, 8);
    let mut parser = felis_vt::Parser::new();
    parser.advance(&mut grid, b"row1\r\nrow2\r\nrow3\r\nrow4\r\nrow5");
    assert_eq!(grid.scrollback().len(), 3);

    let rows = region::region_rows_window(
        &grid,
        0,
        felis_protocol::messages::RegionSource::Scrollback,
        false,
        None,
        0,
        usize::MAX,
    )
    .expect("scrollback always resolves");
    let indexed: Vec<(i32, &str)> = rows.iter().map(|r| (r.row, r.text.as_str())).collect();
    assert_eq!(
        indexed,
        vec![
            (-3, "row1"),
            (-2, "row2"),
            (-1, "row3"),
            (0, "row4"),
            (1, "row5")
        ],
    );
    assert!(
        rows.iter().all(|r| r.ansi.is_none()),
        "ansi rides only when the request asks",
    );
}

/// `max_rows` filters without renumbering (the `capture --lines N` wire
/// path).
#[test]
fn region_rows_window_tail_keeps_indices() {
    let mut grid = Grid::new(2, 8);
    let mut parser = felis_vt::Parser::new();
    parser.advance(&mut grid, b"row1\r\nrow2\r\nrow3\r\nrow4\r\nrow5");
    assert_eq!(grid.scrollback().len(), 3);

    let tail = |cap: u32| {
        region::region_rows_window(
            &grid,
            0,
            felis_protocol::messages::RegionSource::Scrollback,
            false,
            Some(cap),
            0,
            usize::MAX,
        )
        .expect("scrollback always resolves")
        .into_iter()
        .map(|r| (r.row, r.text))
        .collect::<Vec<_>>()
    };

    assert_eq!(
        tail(2),
        vec![(0, "row4".to_string()), (1, "row5".to_string())],
    );
    assert_eq!(tail(0), Vec::<(i32, String)>::new());
    assert_eq!(tail(10).len(), 5);
}

/// `None` becomes an empty stream whose terminator still arrives, so the
/// client's collecting loop never hangs.
#[test]
fn region_rows_indexed_unresolved_mark_range_is_none() {
    let grid = Grid::new(24, 80);
    assert!(
        region::region_rows_window(
            &grid,
            0,
            felis_protocol::messages::RegionSource::CommandOutput,
            false,
            None,
            0,
            usize::MAX,
        )
        .is_none(),
    );
}

/// Steady-browse cycles ship no `RowDelta`: kitty / wezterm freeze the
/// visible content while scrolled back, and felis matches.
#[cfg(unix)]
#[test]
fn compose_diffs_freezes_view_during_steady_browse() {
    let mut grid = Grid::new(2, 8);
    let mut parser = felis_vt::Parser::new();
    parser.advance(&mut grid, b"row1\r\nrow2\r\nrow3\r\nrow4\r\nrow5");

    let mut stream = seeded_stream(&grid);
    stream.diff.last_cursor = felis_grid::Cursor {
        row: 0,
        col: 0,
        visible: false,
        pending_wrap: false,
    };
    stream.diff.viewport = 1;
    stream.diff.viewport_anchor = grid.viewport_anchor();
    stream.diff.last_viewport_state = (1u32, viewport_max_for(&grid), Some(grid.viewport_anchor()));
    stream.diff.composed_browse = Some((grid.viewport_anchor(), 1));
    grid.damage_mut().clear();
    parser.advance(&mut grid, b"more!");
    merge_damage(&mut grid, &mut stream);

    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");

    assert!(
        grid_msgs(&out)
            .iter()
            .all(|m| !matches!(m, GridMsg::RowDelta { .. })),
        "steady browse must not flush PTY-driven RowDeltas — view stays frozen",
    );
    assert!(
        stream.damage.dirty_rows().next().is_some(),
        "damage must remain accumulated for the snap-back flush",
    );
}

fn browse_top_row(out: &[OutEvent]) -> Option<String> {
    grid_msgs(out).iter().find_map(|m| match m {
        GridMsg::RowDelta { rows } => rows
            .iter()
            .find(|(r, _)| *r == 0)
            .map(|(_, payload)| row_text(&payload.0).trim_end().to_owned()),
        _ => None,
    })
}

fn shipped_viewport(out: &[OutEvent]) -> Option<(u32, Option<felis_grid::ViewportAnchor>)> {
    grid_msgs(out).iter().rev().find_map(|m| match m {
        GridMsg::ViewportState {
            lines_from_bottom,
            anchor,
            ..
        } => Some((*lines_from_bottom, *anchor)),
        _ => None,
    })
}

/// A two-row grid holding `l1` .. `l{n}`, browsed at `offset`.
fn browsing_at(
    lines: u32,
    offset: u32,
    scrollback: usize,
) -> (Grid, felis_vt::Parser, SubscriberStream) {
    let mut grid = Grid::with_scrollback(2, 8, scrollback);
    let mut parser = felis_vt::Parser::new();
    let text: Vec<String> = (1..=lines).map(|n| format!("l{n}")).collect();
    parser.advance(&mut grid, text.join("\r\n").as_bytes());
    let mut stream = seeded_stream(&grid);
    grid.damage_mut().clear();
    stream.diff.request_viewport(&grid, offset, None);
    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");
    (grid, parser, stream)
}

#[test]
fn a_browsed_view_stays_on_its_lines_as_output_arrives() {
    let (mut grid, mut parser, mut stream) = browsing_at(8, 2, 100);
    assert_eq!(
        grid.cell_at_viewport(2, 0, 1).unwrap().grapheme,
        felis_grid::Grapheme::Ascii(b'5')
    );
    parser.advance(&mut grid, b"\r\nl9\r\nl10");
    merge_damage(&mut grid, &mut stream);

    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");

    let (offset, anchor) = shipped_viewport(&out).expect("the grown offset is reported");
    assert_eq!(offset, 4);
    assert_eq!(anchor, Some(grid.viewport_anchor()));
    assert_eq!(
        browse_top_row(&out),
        None,
        "the same lines stay on screen; nothing to resend"
    );

    stream.diff.request_viewport(&grid, offset + 1, anchor);
    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");
    assert_eq!(
        browse_top_row(&out).as_deref(),
        Some("l4"),
        "one line above l5"
    );
}

#[test]
fn a_step_computed_before_the_grown_offset_arrived_still_moves_one_line() {
    let (mut grid, mut parser, mut stream) = browsing_at(8, 2, 100);
    let seen = grid.viewport_anchor();
    parser.advance(&mut grid, b"\r\nl9\r\nl10");
    merge_damage(&mut grid, &mut stream);
    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");

    // Computed from offset 2 at the anchor the client last saw.
    stream.diff.request_viewport(&grid, 3, Some(seen));
    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");
    assert_eq!(browse_top_row(&out).as_deref(), Some("l4"));
}

#[test]
fn an_unanchored_step_counts_from_the_current_bottom() {
    let (mut grid, mut parser, mut stream) = browsing_at(8, 0, 100);
    parser.advance(&mut grid, b"\r\nl9\r\nl10");
    merge_damage(&mut grid, &mut stream);
    stream.diff.request_viewport(&grid, 1, None);
    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");
    assert_eq!(browse_top_row(&out).as_deref(), Some("l8"));
}

#[test]
fn an_anchored_zero_joins_the_live_view() {
    let (mut grid, mut parser, mut stream) = browsing_at(8, 1, 100);
    let seen = grid.viewport_anchor();
    parser.advance(&mut grid, b"\r\nl9");
    assert!(stream.diff.request_viewport(&grid, 0, Some(seen)));
    assert_eq!(stream.diff.viewport, 0);
}

#[test]
fn eviction_under_a_browsed_view_recomposes_it_and_ships_a_fresh_anchor() {
    let (mut grid, mut parser, mut stream) = browsing_at(8, 3, 3);
    assert_eq!(
        stream.diff.viewport, 3,
        "browsing at the oldest retained row"
    );
    let before = stream.diff.last_viewport_state;
    parser.advance(&mut grid, b"\r\nl9");
    merge_damage(&mut grid, &mut stream);

    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");

    let (offset, anchor) = shipped_viewport(&out).expect("a fresh anchor ships");
    assert_eq!(offset, before.0, "still the oldest row");
    assert_ne!(anchor, before.2);
    assert_eq!(
        browse_top_row(&out).as_deref(),
        Some("l5"),
        "l4 was evicted"
    );

    stream.diff.request_viewport(&grid, offset - 1, anchor);
    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");
    assert_eq!(
        browse_top_row(&out).as_deref(),
        Some("l6"),
        "line-down moves one line"
    );
}

#[test]
fn a_resize_under_a_browsed_view_returns_it_to_live() {
    let (mut grid, _parser, mut stream) = browsing_at(8, 2, 100);
    grid.resize(2, 6);
    merge_damage(&mut grid, &mut stream);
    stream.catch_up_to(&grid);
    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");
    assert_eq!(shipped_viewport(&out), Some((0, None)));
    let resent = grid_msgs(&out)
        .iter()
        .filter_map(|m| match m {
            GridMsg::RowDelta { rows } => Some(rows.len()),
            _ => None,
        })
        .sum::<usize>();
    assert_eq!(resent, 2, "every live row replaces the browse rows");
}

#[test]
fn an_erased_and_refilled_history_returns_a_browsed_view_to_live() {
    let (mut grid, mut parser, mut stream) = browsing_at(8, 2, 100);
    parser.advance(&mut grid, b"\x1b[3J\r\nx1\r\nx2\r\nx3\r\nx4");
    merge_damage(&mut grid, &mut stream);
    let mut out = Vec::new();
    compose_one(&mut grid, &mut stream, &mut out).expect("compose ok");
    assert_eq!(shipped_viewport(&out), Some((0, None)));
}

/// A subscriber that still owes rows in the band takes the directive
/// too, its owed rows moving with the shift.
#[test]
fn a_subscriber_with_unshipped_rows_takes_the_directive_with_its_rows_moved() {
    let op = felis_grid::ScrollOp {
        region_top: 0,
        region_bottom: 3,
        n_rows: 1,
        direction: felis_grid::ScrollDirection::Up,
    };
    let grid = Grid::new(4, 8);

    let mut clean = seeded_stream(&grid);
    clean.accept_scroll(op);
    assert_eq!(clean.pending_scrolls, vec![op]);
    assert_eq!(clean.damage.dirty_rows().collect::<Vec<_>>(), vec![3]);

    let mut behind = seeded_stream(&grid);
    behind.damage.mark(2);
    behind.accept_scroll(op);
    assert_eq!(behind.pending_scrolls, vec![op]);
    assert_eq!(behind.damage.dirty_rows().collect::<Vec<_>>(), vec![1, 3]);
}

/// Shifts of one band fold into one directive until they would blank it,
/// when the replay restates the band and the directive is dropped.
#[test]
fn shifts_of_one_band_fold_until_the_band_is_owed_whole() {
    let op = |n_rows| felis_grid::ScrollOp {
        region_top: 1,
        region_bottom: 3,
        n_rows,
        direction: felis_grid::ScrollDirection::Up,
    };
    let grid = Grid::new(5, 8);
    let mut stream = seeded_stream(&grid);
    stream.accept_scroll(op(1));
    stream.accept_scroll(op(1));
    assert_eq!(stream.pending_scrolls, vec![op(2)]);
    stream.accept_scroll(op(1));
    assert_eq!(stream.pending_scrolls, Vec::<felis_grid::ScrollOp>::new());
    assert_eq!(
        stream.damage.dirty_rows().collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
}

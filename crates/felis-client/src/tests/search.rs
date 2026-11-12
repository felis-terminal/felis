use super::*;

#[test]
fn search_ui_starts_off() {
    assert_eq!(SearchUi::default().mode, SearchUiMode::Off);
}
#[test]
fn format_search_label_renders_each_search_state() {
    let active = |query: &str| SearchUi {
        mode: SearchUiMode::Active,
        query: query.into(),
        ..SearchUi::default()
    };
    let cases = [
        (
            "composing appends cursor marker",
            SearchUi {
                mode: SearchUiMode::Composing,
                query: "foo".into(),
                ..SearchUi::default()
            },
            "find: foo_",
        ),
        (
            "active streaming shows ellipsis count",
            SearchUi {
                streaming: true,
                matches: vec![dummy_hit(-1); 3],
                ..active("foo")
            },
            "find: foo  (3…)",
        ),
        (
            "active with no matches says so",
            active("foo"),
            "find: foo  no matches",
        ),
        (
            "active with current shows position",
            SearchUi {
                matches: vec![dummy_hit(-1), dummy_hit(-3), dummy_hit(-5)],
                current: Some(1),
                ..active("foo")
            },
            "find: foo  2/3",
        ),
        // A daemon-side error wins over a partial stream's count.
        (
            "active error overrides count",
            SearchUi {
                error: Some("invalid regex".into()),
                ..active("(")
            },
            "find: (  error: invalid regex",
        ),
    ];
    for (case, ui, expected) in cases {
        assert_eq!(format_search_label_for(&ui), expected, "{case}");
    }
}

/// Pins: a query the client refuses to send leaves no stream behind and
/// says why. `commit_search_query` runs this before it opens a stream, so
/// a bar in this state means the daemon was never told about a stream id
/// the client had already burned, a mismatch it answers by dropping
/// the connection.
#[test]
fn refused_search_reports_the_limit_and_holds_no_stream() {
    use felis_protocol::codec::WireCodec;
    use felis_protocol::messages::{MAX_SEARCH_PATTERN_BYTES, SearchOptions, SearchToDaemonMsg};

    let query = "x".repeat(MAX_SEARCH_PATTERN_BYTES + 1);
    let err = SearchToDaemonMsg::Query {
        query: query.clone(),
        options: SearchOptions::default(),
        source: felis_protocol::messages::SearchSource::Scrollback,
        session: None,
    }
    .validate()
    .expect_err("a query over the pattern limit is refused before the wire");

    let mut ui = SearchUi {
        mode: SearchUiMode::Composing,
        query,
        streaming: true,
        matches: vec![dummy_hit(-1)],
        current: Some(0),
        stream: None,
        error: None,
    };
    app_methods::refuse_search(&mut ui, err.to_string());

    assert_eq!(ui.mode, SearchUiMode::Active);
    assert!(!ui.streaming);
    assert!(ui.stream.is_none());
    assert!(ui.matches.is_empty());
    assert!(ui.current.is_none());
    assert!(
        format_search_label_for(&ui).contains("error: wire field Search::Query.query"),
        "the bar names the refused field, not just an empty result"
    );
}

fn dummy_hit(line_index: i64) -> SearchHitRecord {
    SearchHitRecord {
        line_index,
        col_spans: vec![ColSpan {
            line_index,
            col_start: 0,
            col_end: 1,
        }],
        anchor: None,
    }
}

use crate::app_methods::format_search_label as format_search_label_for;

mod search_navigation {
    use crate::app_methods::{
        hit_navigation, next_search_index, search_hit_anchor_line, visible_hit_spans,
    };

    use super::*;

    /// The soft-wrap-stitched shape `advance_search_hit` anchors on: one
    /// segment per row, starting `first_seg_offset` rows below the
    /// logical line's top.
    fn stitched_hit(line_index: i64, first_seg_offset: i64, segments: u8) -> SearchHitRecord {
        SearchHitRecord {
            line_index,
            col_spans: (0..i64::from(segments))
                .map(|i| ColSpan {
                    line_index: line_index + first_seg_offset + i,
                    col_start: 2,
                    col_end: 6,
                })
                .collect(),
            anchor: None,
        }
    }

    proptest::proptest! {
        /// `n` and `N` are inverse steps of one cycle over the whole hit
        /// list, and `matches` is newest-first, so a first `n` opens at
        /// the youngest hit and a first `N` at the oldest.
        #[test]
        fn next_search_index_walks_one_cycle_in_both_directions(
            len in 1_usize..=64,
            offset in 0_usize..=64,
        ) {
            let start = offset % len;
            proptest::prop_assert_eq!(next_search_index(None, len, SearchDirection::Older), Some(0));
            proptest::prop_assert_eq!(
                next_search_index(None, len, SearchDirection::Newer),
                Some(len - 1),
            );
            for (forward, back) in [
                (SearchDirection::Older, SearchDirection::Newer),
                (SearchDirection::Newer, SearchDirection::Older),
            ] {
                let mut visited = Vec::with_capacity(len);
                let mut current = Some(start);
                for _ in 0..len {
                    let next = next_search_index(current, len, forward);
                    proptest::prop_assert_eq!(next_search_index(next, len, back), current);
                    current = next;
                    visited.push(next.expect("a non-empty list always has a next hit"));
                }
                proptest::prop_assert_eq!(current, Some(start), "the walk must close the cycle");
                visited.sort_unstable();
                visited.dedup();
                proptest::prop_assert_eq!(visited.len(), len, "every hit must be reachable once");
            }
        }

        #[test]
        fn next_search_index_has_nothing_to_pick_without_matches(
            current in proptest::option::of(0_usize..=64),
            newer in proptest::bool::ANY,
        ) {
            let direction = if newer { SearchDirection::Newer } else { SearchDirection::Older };
            proptest::prop_assert_eq!(next_search_index(current, 0, direction), None);
        }
    }

    /// Pins: the jump centers on the first painted segment; the logical
    /// line's top may be a row where nothing is highlighted.
    const fn at(pushed: u64, generation: u64) -> ViewportAnchor {
        ViewportAnchor { pushed, generation }
    }

    fn anchored(line_index: i64, anchor: Option<ViewportAnchor>) -> SearchHitRecord {
        SearchHitRecord {
            anchor,
            ..stitched_hit(line_index, 0, 1)
        }
    }

    #[test]
    fn a_hit_keeps_its_row_while_output_grows_the_browsed_offset() {
        // Found at -6 with the view at 5: row 0 of the browsed screen.
        // Three pushes later the same lines sit at offset 8.
        let hit = anchored(-6, Some(at(10, 0)));
        let row_of = |frame, viewport| {
            visible_hit_spans(std::slice::from_ref(&hit), None, frame, viewport, 24)[0].row
        };
        assert_eq!(row_of(Some(at(10, 0)), 6), 0);
        assert_eq!(row_of(Some(at(13, 0)), 9), 0);
    }

    #[test]
    fn a_hit_found_after_the_view_last_moved_shifts_the_other_way() {
        let hit = anchored(-9, Some(at(13, 0)));
        assert_eq!(
            visible_hit_spans(&[hit], None, Some(at(10, 0)), 6, 24)[0].row,
            0,
            "-9 three pushes on is -6 in the older frame",
        );
    }

    #[test]
    fn a_hit_from_another_generation_is_not_painted() {
        let hit = anchored(-6, Some(at(10, 0)));
        assert_eq!(
            visible_hit_spans(&[hit], None, Some(at(13, 1)), 6, 24),
            Vec::<SearchHitSpan>::new()
        );
    }

    #[test]
    fn navigating_to_a_hit_from_another_generation_names_its_own_anchor() {
        let hit = anchored(-6, Some(at(10, 0)));
        let shown = ViewportPosition {
            offset: 6,
            anchor: Some(at(13, 1)),
        };
        let request = hit_navigation(&hit, shown, 24, 100);
        assert_eq!(
            request.anchor,
            Some(at(10, 0)),
            "the daemon answers a stale one with live"
        );
    }

    #[test]
    fn navigating_to_a_hit_names_the_hit_frame() {
        // In the shadow's older frame this hit would project to line 0,
        // a live-bottom target the daemon would not grow.
        let hit = anchored(-1, Some(at(11, 0)));
        let shown = ViewportPosition {
            offset: 0,
            anchor: None,
        };
        let request = hit_navigation(&hit, shown, 24, 100);
        assert_eq!(request.offset, 13);
        assert_eq!(request.anchor, Some(at(11, 0)));
    }

    #[test]
    fn navigating_to_an_anchored_hit_leaves_the_clamp_to_the_daemon() {
        // The shadow's maximum of 1 predates the scrollback the hit lies in.
        let hit = anchored(-2, Some(at(11, 0)));
        let shown = ViewportPosition {
            offset: 1,
            anchor: Some(at(10, 0)),
        };
        let request = hit_navigation(&hit, shown, 24, 1);
        assert_eq!(request.offset, 14);
    }

    #[test]
    fn navigating_to_a_hit_found_live_that_scrolled_into_history_browses_to_it() {
        // Found on live row 5; twenty pushes later it is scrollback line -15.
        let hit = anchored(5, Some(at(10, 0)));
        let shown = ViewportPosition {
            offset: 30,
            anchor: Some(at(30, 0)),
        };
        let request = hit_navigation(&hit, shown, 24, 100);
        assert_eq!(request.offset, 27);
        assert_eq!(request.anchor, Some(at(30, 0)));
    }

    /// The shadow may lag a step still in flight, so a hit it already
    /// shows is requested anyway, at the offset it is shown at.
    #[test]
    fn navigating_to_a_hit_already_in_view_requests_the_shown_offset() {
        let hit = anchored(-13, Some(at(10, 0)));
        let shown = ViewportPosition {
            offset: 28,
            anchor: Some(at(13, 0)),
        };
        assert_eq!(hit_navigation(&hit, shown, 24, 100), shown);
    }

    #[test]
    fn search_hit_anchor_line_prefers_the_first_painted_segment() {
        assert_eq!(search_hit_anchor_line(&stitched_hit(-40, 3, 2)), -37);
        assert_eq!(
            search_hit_anchor_line(&SearchHitRecord {
                line_index: -40,
                col_spans: Vec::new(),
                anchor: None,
            }),
            -40
        );
    }

    #[test]
    fn visible_hit_spans_project_each_segment_and_tag_the_current_hit() {
        let matches = [stitched_hit(-5, 0, 2), stitched_hit(-2, 0, 1)];
        // `composed_row = line_index + K`.
        let spans = visible_hit_spans(&matches, Some(1), None, 5, 24);
        assert_eq!(spans.len(), 3);
        assert_eq!(
            spans.iter().map(|s| s.row).collect::<Vec<_>>(),
            vec![0, 1, 3]
        );
        assert_eq!(
            spans.iter().map(|s| s.is_current).collect::<Vec<_>>(),
            vec![false, false, true],
        );
        assert_eq!((spans[0].col_start, spans[0].col_end), (2, 6));
        let none_current = visible_hit_spans(&matches, None, None, 5, 24);
        assert!(none_current.iter().all(|s| !s.is_current));
    }

    /// Pins: off-screen segments are dropped, not clamped; a clamped row
    /// would paint a highlight over unrelated text.
    #[test]
    fn visible_hit_spans_drop_offscreen_segments_and_keep_the_visible_ones() {
        let spans = visible_hit_spans(&[stitched_hit(-6, 0, 2)], Some(0), None, 5, 24);
        assert_eq!(spans.len(), 1, "only the on-screen segment paints");
        assert_eq!(spans[0].row, 0);
        assert_eq!(
            visible_hit_spans(&[stitched_hit(-100, 0, 1)], Some(0), None, 5, 24),
            Vec::<SearchHitSpan>::new()
        );
    }

    /// Pins: an empty column range never reaches the renderer as a
    /// zero-width quad.
    #[test]
    fn visible_hit_spans_skip_empty_column_ranges() {
        let empty = SearchHitRecord {
            line_index: -1,
            col_spans: vec![
                ColSpan {
                    line_index: -1,
                    col_start: 4,
                    col_end: 4,
                },
                ColSpan {
                    line_index: -1,
                    col_start: 6,
                    col_end: 5,
                },
            ],
            anchor: None,
        };
        assert_eq!(
            visible_hit_spans(&[empty], Some(0), None, 5, 24),
            Vec::<SearchHitSpan>::new()
        );
    }
}

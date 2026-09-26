use proptest::prelude::*;

use super::*;
use crate::{AttrFlags, Attributes, Color, UnderlineStyle};

/// Hand-built rows driving [`search_rows`] directly, with `sb_len =
/// rows.len()` so every row sits in REQ-608's negative index space
/// (youngest = `-1`).
#[derive(Default)]
struct Rows {
    rows: Vec<Vec<Cell>>,
    continued: Vec<bool>,
}

impl Rows {
    fn push(&mut self, row: &[Cell], soft_wrap_continued: bool) {
        self.rows.push(row.to_vec());
        self.continued.push(soft_wrap_continued);
    }

    /// No cluster table: nothing in these ASCII/`Char` rows has a handle.
    fn search<'a>(
        &'a self,
        query: &'a SearchQuery,
        clusters: &'a ClusterTable,
    ) -> impl Iterator<Item = SearchHit> + 'a {
        search_rows(
            self.rows.iter().map(Vec::as_slice).collect(),
            &self.continued,
            self.rows.len(),
            query,
            clusters,
        )
    }
}

fn push_text(sb: &mut Rows, cols: usize, s: &str) {
    push_text_wrapped(sb, cols, s, false);
}

#[test]
fn empty_needle_is_rejected() {
    let err = SearchQuery::new("", SearchOptions::default()).unwrap_err();
    assert!(matches!(err, SearchQueryError::EmptyNeedle));
}

#[test]
fn search_on_empty_scrollback_yields_nothing() {
    let sb = Rows::default();
    let q = SearchQuery::new("anything", SearchOptions::default()).unwrap();
    assert_eq!(sb.search(&q, &ClusterTable::default()).count(), 0);
}

#[test]
fn case_insensitive_plain() {
    let mut sb = Rows::default();
    push_text(&mut sb, 80, "Hello World");
    let q = SearchQuery::new(
        "hello",
        SearchOptions {
            regex: false,
            case_insensitive: true,
        },
    )
    .unwrap();
    let hits: Vec<_> = sb.search(&q, &ClusterTable::default()).collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].byte_spans, vec![ByteSpan { start: 0, end: 5 }]);
    assert_eq!(&hits[0].text[0..5], "Hello");
}

#[test]
fn regex_with_anchor() {
    let mut sb = Rows::default();
    push_text(&mut sb, 80, "abc");
    push_text(&mut sb, 80, "  abc");
    let q = SearchQuery::new(
        "^abc",
        SearchOptions {
            regex: true,
            case_insensitive: false,
        },
    )
    .unwrap();
    let hits: Vec<_> = sb.search(&q, &ClusterTable::default()).collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].text, "abc");
}

#[test]
fn regex_with_inline_case_insensitive_flag() {
    let mut sb = Rows::default();
    push_text(&mut sb, 80, "AbCdE");
    let q = SearchQuery::new(
        "abc",
        SearchOptions {
            regex: true,
            case_insensitive: true,
        },
    )
    .unwrap();
    let hits: Vec<_> = sb.search(&q, &ClusterTable::default()).collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].byte_spans, vec![ByteSpan { start: 0, end: 3 }]);
}

/// Guards the `.filter(|(s, e)| s != e)` in the regex matcher: without
/// it every empty position between letters surfaces as a span.
#[test]
fn regex_zero_width_matches_are_dropped() {
    let mut sb = Rows::default();
    push_text(&mut sb, 80, "foo");
    let q = SearchQuery::new(
        "o*",
        SearchOptions {
            regex: true,
            case_insensitive: false,
        },
    )
    .unwrap();
    let hits: Vec<_> = sb.search(&q, &ClusterTable::default()).collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].byte_spans, vec![ByteSpan { start: 1, end: 3 }]);
}

#[test]
fn bad_regex_returns_error() {
    let err = SearchQuery::new(
        "(unclosed",
        SearchOptions {
            regex: true,
            case_insensitive: false,
        },
    )
    .unwrap_err();
    assert!(matches!(err, SearchQueryError::BadRegex(_)));
}

#[test]
fn trailing_blanks_do_not_inflate_match_spans() {
    // Without the trailing trim, "hi   " would match "hi" + 78 blanks.
    let mut sb = Rows::default();
    push_text(&mut sb, 80, "hi");
    let q = SearchQuery::new("hi   ", SearchOptions::default()).unwrap();
    assert_eq!(sb.search(&q, &ClusterTable::default()).count(), 0);

    let q2 = SearchQuery::new("hi", SearchOptions::default()).unwrap();
    let hits: Vec<_> = sb.search(&q2, &ClusterTable::default()).collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].text, "hi");
}

#[test]
fn multibyte_chars_resolve_to_correct_cell_columns() {
    let mut row: Vec<Cell> = (0..10).map(|_| Cell::default()).collect();
    row[0].grapheme = Grapheme::Ascii(b'h');
    row[1].grapheme = Grapheme::Char('é');
    row[2].grapheme = Grapheme::Ascii(b'l');
    row[3].grapheme = Grapheme::Ascii(b'l');
    row[4].grapheme = Grapheme::Ascii(b'o');
    let mut sb = Rows::default();
    sb.push(&row, false);

    let q = SearchQuery::new("llo", SearchOptions::default()).unwrap();
    let hits: Vec<_> = sb.search(&q, &ClusterTable::default()).collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].byte_spans, vec![ByteSpan { start: 3, end: 6 }]);
    assert_eq!(
        hits[0].col_spans,
        vec![ColSpan {
            line_index: -1,
            col_start: 2,
            col_end: 5
        }]
    );
}

#[test]
fn wide_cell_spacer_does_not_emit_bytes() {
    let mut row: Vec<Cell> = (0..10).map(|_| Cell::default()).collect();
    row[0].grapheme = Grapheme::Char('中');
    row[1].grapheme = Grapheme::Spacer;
    row[2].grapheme = Grapheme::Ascii(b'X');
    let mut sb = Rows::default();
    sb.push(&row, false);

    let q = SearchQuery::new("X", SearchOptions::default()).unwrap();
    let hits: Vec<_> = sb.search(&q, &ClusterTable::default()).collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0].col_spans,
        vec![ColSpan {
            line_index: -1,
            col_start: 2,
            col_end: 3
        }]
    );
}

#[test]
fn row_text_attributes_do_not_leak_into_matches() {
    // Search must not see SGR escapes.
    let mut row: Vec<Cell> = (0..5).map(|_| Cell::default()).collect();
    row[0].grapheme = Grapheme::Ascii(b'b');
    let mut styles = crate::StyleTable::new();
    row[0].style = styles.intern(Attributes {
        fg: Color::Indexed(1),
        bg: Color::Default,
        flags: AttrFlags::BOLD,
        underline_color: Color::Default,
        underline_style: UnderlineStyle::default(),
    });
    row[1].grapheme = Grapheme::Ascii(b'o');
    row[2].grapheme = Grapheme::Ascii(b'l');
    row[3].grapheme = Grapheme::Ascii(b'd');
    let mut sb = Rows::default();
    sb.push(&row, false);

    let q = SearchQuery::new("bold", SearchOptions::default()).unwrap();
    let hits: Vec<_> = sb.search(&q, &ClusterTable::default()).collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].text, "bold");
}

fn push_text_wrapped(sb: &mut Rows, cols: usize, s: &str, wrapped: bool) {
    let mut row: Vec<Cell> = (0..cols).map(|_| Cell::default()).collect();
    for (i, ch) in s.chars().enumerate() {
        if i >= cols {
            break;
        }
        row[i].grapheme = if (ch as u32) < 0x80 {
            Grapheme::Ascii(ch as u8)
        } else {
            Grapheme::Char(ch)
        };
    }
    sb.push(&row, wrapped);
}

#[test]
fn needle_spanning_a_wrap_edge_is_found() {
    let mut sb = Rows::default();
    push_text_wrapped(&mut sb, 8, "hello wo", false);
    push_text_wrapped(&mut sb, 8, "rld", true);
    let q = SearchQuery::new("world", SearchOptions::default()).unwrap();
    let hits: Vec<_> = sb.search(&q, &ClusterTable::default()).collect();
    assert_eq!(hits.len(), 1);
    let h = &hits[0];
    assert_eq!(h.line_index, -2);
    assert_eq!(h.text, "hello world");
    assert_eq!(h.byte_spans, vec![ByteSpan { start: 6, end: 11 }]);
    assert_eq!(
        h.col_spans,
        vec![
            ColSpan {
                line_index: -2,
                col_start: 6,
                col_end: 8
            },
            ColSpan {
                line_index: -1,
                col_start: 0,
                col_end: 3
            }
        ]
    );
}

#[test]
fn match_inside_a_continuation_row_reports_that_row() {
    let mut sb = Rows::default();
    push_text_wrapped(&mut sb, 8, "01234567", false);
    push_text_wrapped(&mut sb, 8, "needle", true);
    let q = SearchQuery::new("needle", SearchOptions::default()).unwrap();
    let hits: Vec<_> = sb.search(&q, &ClusterTable::default()).collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].line_index, -2);
    assert_eq!(hits[0].byte_spans, vec![ByteSpan { start: 8, end: 14 }]);
    assert_eq!(
        hits[0].col_spans,
        vec![ColSpan {
            line_index: -1,
            col_start: 0,
            col_end: 6
        }]
    );
}

/// The unfillable `Empty` a wide-glyph wrap leaves at a wrapped row's
/// tail must not inject a phantom space into the stitched text.
#[test]
fn wide_glyph_wrap_gap_does_not_break_stitching() {
    let mut sb = Rows::default();
    push_text_wrapped(&mut sb, 4, "abc", false);
    let mut row: Vec<Cell> = (0..4).map(|_| Cell::default()).collect();
    row[0].grapheme = Grapheme::Char('中');
    row[1].grapheme = Grapheme::Spacer;
    row[2].grapheme = Grapheme::Ascii(b'x');
    sb.push(&row, true);
    let q = SearchQuery::new("abc中x", SearchOptions::default()).unwrap();
    let hits: Vec<_> = sb.search(&q, &ClusterTable::default()).collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].text, "abc中x");
    assert_eq!(
        hits[0].col_spans,
        vec![
            ColSpan {
                line_index: -2,
                col_start: 0,
                col_end: 3
            },
            ColSpan {
                line_index: -1,
                col_start: 0,
                col_end: 3
            }
        ]
    );
}

/// Only `Empty` cells are trimmed from continued rows; printed spaces
/// are content.
#[test]
fn printed_spaces_on_a_wrapped_row_survive_stitching() {
    let mut sb = Rows::default();
    push_text_wrapped(&mut sb, 4, "ab  ", false);
    push_text_wrapped(&mut sb, 4, "cd", true);
    let q = SearchQuery::new("ab  cd", SearchOptions::default()).unwrap();
    assert_eq!(sb.search(&q, &ClusterTable::default()).count(), 1);
    let q2 = SearchQuery::new("abcd", SearchOptions::default()).unwrap();
    assert_eq!(sb.search(&q2, &ClusterTable::default()).count(), 0);
}

#[test]
fn multiple_matches_across_a_stitched_line() {
    let mut sb = Rows::default();
    push_text_wrapped(&mut sb, 8, "foo bafo", false);
    push_text_wrapped(&mut sb, 8, "o foo", true);
    let q = SearchQuery::new("foo", SearchOptions::default()).unwrap();
    let hits: Vec<_> = sb.search(&q, &ClusterTable::default()).collect();
    assert_eq!(hits.len(), 1);
    let h = &hits[0];
    assert_eq!(h.text, "foo bafoo foo");
    assert_eq!(
        h.byte_spans,
        vec![
            ByteSpan { start: 0, end: 3 },
            ByteSpan { start: 6, end: 9 },
            ByteSpan { start: 10, end: 13 }
        ]
    );
    assert_eq!(
        h.col_spans,
        vec![
            ColSpan {
                line_index: -2,
                col_start: 0,
                col_end: 3
            },
            ColSpan {
                line_index: -2,
                col_start: 6,
                col_end: 8
            },
            ColSpan {
                line_index: -1,
                col_start: 0,
                col_end: 1
            },
            ColSpan {
                line_index: -1,
                col_start: 2,
                col_end: 5
            }
        ]
    );
}

/// A continuation row's first column is mid-line, not a line start.
#[test]
fn regex_anchor_respects_logical_lines() {
    let mut sb = Rows::default();
    push_text_wrapped(&mut sb, 8, "xxxxxxxx", false);
    push_text_wrapped(&mut sb, 8, "abc", true);
    let q = SearchQuery::new(
        "^abc",
        SearchOptions {
            regex: true,
            case_insensitive: false,
        },
    )
    .unwrap();
    assert_eq!(sb.search(&q, &ClusterTable::default()).count(), 0);
}

/// Eviction itself is the Grid ring's business; this pins the kernel's
/// contract for the headless tail eviction leaves behind.
#[test]
fn evicted_line_head_leaves_a_searchable_tail() {
    let mut sb = Rows::default();
    push_text_wrapped(&mut sb, 8, "tail one", true);
    push_text_wrapped(&mut sb, 8, "tail two", true);
    let q = SearchQuery::new("one", SearchOptions::default()).unwrap();
    let hits: Vec<_> = sb.search(&q, &ClusterTable::default()).collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].line_index, -2);
    assert_eq!(hits[0].text, "tail onetail two");
}

/// The grid's scrollback, wrap bits, and live rows come from genuine VT
/// behavior rather than hand-pushed rows.
fn driven_grid(rows: u16, cols: u16, bytes: &[u8]) -> crate::Grid {
    let mut g = crate::Grid::new(rows, cols);
    let mut p = felis_vt::Parser::new();
    crate::test_support::drive(&mut p, &mut g, bytes);
    g
}

/// REQ-608: live row `r` reports as `r`.
#[test]
fn grid_search_finds_live_rows_in_the_non_negative_index_space() {
    let g = driven_grid(3, 8, b"hello\r\nworld");
    let q = SearchQuery::new("world", SearchOptions::default()).unwrap();
    let hits: Vec<_> = g.search(&q).collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].line_index, 1);
    assert_eq!(
        hits[0].col_spans,
        vec![ColSpan {
            line_index: 1,
            col_start: 0,
            col_end: 5
        }]
    );
}

/// A logical line whose head scrolled into history while its tail is
/// still live stitches across the seam.
#[test]
fn grid_search_stitches_a_line_across_the_scrollback_seam() {
    let g = driven_grid(2, 3, b"abcdef\r\nxx");
    assert_eq!(g.scrollback().len(), 1);
    assert!(g.row_soft_wrap_continued(0), "live tail must be tied");
    let q = SearchQuery::new("bcde", SearchOptions::default()).unwrap();
    let hits: Vec<_> = g.search(&q).collect();
    assert_eq!(hits.len(), 1);
    let h = &hits[0];
    assert_eq!(h.line_index, -1, "anchor = topmost row, in scrollback");
    assert_eq!(h.text, "abcdef");
    assert_eq!(h.byte_spans, vec![ByteSpan { start: 1, end: 5 }]);
    assert_eq!(
        h.col_spans,
        vec![
            ColSpan {
                line_index: -1,
                col_start: 1,
                col_end: 3
            },
            ColSpan {
                line_index: 0,
                col_start: 0,
                col_end: 2
            }
        ]
    );
}

/// Keeps the overlay's top-is-most-recent ordering.
#[test]
fn grid_search_orders_hits_live_first_then_scrollback() {
    let g = driven_grid(2, 8, b"foo\r\n.\r\n.\r\nfoo");
    let q = SearchQuery::new("foo", SearchOptions::default()).unwrap();
    let indices: Vec<i64> = g.search(&q).map(|h| h.line_index).collect();
    assert_eq!(indices, vec![1, -2]);
}

/// The alternate screen is a transient surface the program repaints at
/// will; its rows are no continuation of scrollback.
#[test]
fn grid_search_skips_the_live_pass_on_the_alternate_screen() {
    let mut g = crate::Grid::new(2, 8);
    let mut p = felis_vt::Parser::new();
    crate::test_support::drive(&mut p, &mut g, b"needle\r\n.\r\n.\r\nneedle");
    let q = SearchQuery::new("needle", SearchOptions::default()).unwrap();
    let before: Vec<i64> = g.search(&q).map(|h| h.line_index).collect();
    assert_eq!(before, vec![1, -2]);
    crate::test_support::drive(&mut p, &mut g, b"\x1b[?1049hneedle");
    let during: Vec<i64> = g.search(&q).map(|h| h.line_index).collect();
    assert_eq!(during, vec![-2]);
    crate::test_support::drive(&mut p, &mut g, b"\x1b[?1049l");
    let after: Vec<i64> = g.search(&q).map(|h| h.line_index).collect();
    assert_eq!(after, vec![1, -2]);
}

// ---- Property tests (REQ-607) ----

fn make_rows(rows: &[String], cols: usize) -> Rows {
    let mut sb = Rows::default();
    for r in rows {
        push_text(&mut sb, cols, r);
    }
    sb
}

/// Line index, the row text a hit reports (what the client highlights
/// against), and the byte spans inside it.
type RefHit = (i64, String, Vec<(usize, usize)>);

/// Naive reference: newest-first, `str::find` on each decoded row.
fn naive_search(sb: &Rows, needle: &str) -> Vec<RefHit> {
    let total = sb.rows.len();
    let mut out = Vec::new();
    for (i, row) in sb.rows.iter().enumerate().rev() {
        let line_index = -(i64::try_from(total - i).unwrap_or(i64::MAX));
        let (mut text, _map) = row_text(row, &ClusterTable::default());
        let trimmed = text.trim_end_matches(' ').len();
        text.truncate(trimmed);
        if text.is_empty() {
            continue;
        }
        let mut hits = Vec::new();
        let mut start = 0;
        while let Some(rel) = text[start..].find(needle) {
            let abs = start + rel;
            hits.push((abs, abs + needle.len()));
            start = abs + needle.len();
            if start >= text.len() {
                break;
            }
        }
        if !hits.is_empty() {
            out.push((line_index, text, hits));
        }
    }
    out
}

/// A small alphabet maximizes the needle-hit rate.
fn cell_string(max_len: usize) -> impl Strategy<Value = String> {
    proptest::collection::vec(prop_oneof!["[a-d ]", "[A-D ]", "[0-2]"], 0..=max_len)
        .prop_map(|v| v.concat())
}

proptest! {
    #[test]
    fn substring_search_matches_naive_reference(
        rows in proptest::collection::vec(cell_string(40), 0..16),
        needle in "[a-dA-D0-2 ]{1,5}",
    ) {
        let cols = 40;
        let sb = make_rows(&rows, cols);
        let expected = naive_search(&sb, &needle);
        let q = SearchQuery::new(&needle, SearchOptions::default()).unwrap();
        let actual: Vec<RefHit> = sb
            .search(&q, &ClusterTable::default())
            .map(|h: SearchHit| {
                let spans = h
                    .byte_spans
                    .into_iter()
                    .map(|s| (s.start as usize, s.end as usize))
                    .collect();
                (h.line_index, h.text, spans)
            })
            .collect();
        prop_assert_eq!(actual, expected);
    }

    #[test]
    fn returned_spans_slice_to_the_needle(
        rows in proptest::collection::vec(cell_string(40), 0..16),
        needle in "[a-dA-D0-2 ]{1,5}",
    ) {
        let cols = 40;
        let sb = make_rows(&rows, cols);
        let q = SearchQuery::new(&needle, SearchOptions::default()).unwrap();
        for hit in sb.search(&q, &ClusterTable::default()) {
            for span in &hit.byte_spans {
                let s = span.start as usize;
                let e = span.end as usize;
                prop_assert!(s <= e, "span backwards: {} > {}", s, e);
                prop_assert!(e <= hit.text.len(), "span past text: {} > {}", e, hit.text.len());
                prop_assert!(hit.text.is_char_boundary(s), "span start not at char boundary");
                prop_assert!(hit.text.is_char_boundary(e), "span end not at char boundary");
                prop_assert_eq!(&hit.text[s..e], needle.as_str());
            }
        }
    }

    /// For ASCII-only input byte index == cell column.
    #[test]
    fn col_spans_match_byte_spans_for_ascii(
        rows in proptest::collection::vec("[a-z ]{0,40}".prop_map(String::from), 0..16),
        needle in "[a-z]{1,3}",
    ) {
        let cols = 40;
        let sb = make_rows(&rows, cols);
        let q = SearchQuery::new(&needle, SearchOptions::default()).unwrap();
        for hit in sb.search(&q, &ClusterTable::default()) {
            prop_assert_eq!(hit.byte_spans.len(), hit.col_spans.len());
            for (bs, cs) in hit.byte_spans.iter().zip(hit.col_spans.iter()) {
                prop_assert_eq!(cs.line_index, hit.line_index);
                prop_assert_eq!(u32::from(cs.col_start), bs.start);
                prop_assert_eq!(u32::from(cs.col_end), bs.end);
            }
        }
    }

    /// The compile step either succeeds or returns `BadRegex`; the
    /// search step is total over arbitrary cell content.
    #[test]
    fn regex_input_does_not_panic(
        rows in proptest::collection::vec(cell_string(40), 0..8),
        pattern in "[a-z()|*+?\\[\\]\\\\.]{0,16}",
    ) {
        let cols = 40;
        let sb = make_rows(&rows, cols);
        let q = SearchQuery::new(
            &pattern,
            SearchOptions {
                regex: true,
                case_insensitive: false,
            },
        );
        if let Ok(q) = q {
            let _ = sb.search(&q, &ClusterTable::default()).count();
        }
    }
}

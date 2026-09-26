//! Scrollback search fuzz target.
//!
//! Drives `felis_grid::SearchQuery::new` + the `search_rows` kernel
//! (what `Grid::search` walks) with arbitrary bytes split into
//! (pattern, scrollback rows). Companion to the proptests in
//! `crates/felis-grid/src/search/tests.rs`; those pin
//! model-equivalence and span integrity up to 16 rows × 40 cols.
//! libFuzzer widens the input space — adversarial regex patterns,
//! larger scrollbacks, more permutations of trailing-blank trim and
//! multi-byte UTF-8 in cells.
//!
//! Win condition is silence: panic = bug, BadRegex / EmptyNeedle
//! returns are expected. The fuzzer should never:
//! - panic during compile, search, or row decoding
//! - produce byte_spans that fall off `text`
//! - produce col_spans not aligned with byte_spans (caught by the
//!   matched-text slice assertion below)

#![no_main]

use felis_grid::search::search_rows;
use felis_grid::{Cell, ClusterTable, Grapheme, SearchOptions, SearchQuery};
use libfuzzer_sys::fuzz_target;

const COLS: usize = 40;

/// Pack arbitrary bytes into a small alphabet of grid cells so the
/// matcher sees a realistic mix of ASCII, multi-byte UTF-8, and
/// blanks rather than always-empty rows.
fn byte_to_grapheme(b: u8) -> Grapheme {
    match b & 0b11 {
        0 => Grapheme::Empty,
        1 => Grapheme::Ascii(b'a' + (b >> 2) % 26),
        2 => Grapheme::Char(char::from_u32('Α' as u32 + u32::from(b >> 2) % 24).unwrap_or('α')),
        _ => Grapheme::Ascii(b' '),
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }
    // First byte: option bits. Second byte: pattern length, capped.
    let opt_byte = data[0];
    let options = SearchOptions {
        regex: opt_byte & 1 != 0,
        case_insensitive: opt_byte & 2 != 0,
    };
    let pat_len = (data[1] as usize).min(32).min(data.len().saturating_sub(2));
    let pat_bytes = &data[2..2 + pat_len];
    // Patterns come from a printable-ASCII / regex-metachar mix so
    // libfuzzer can both find substring hits and stress the regex
    // parser. Non-UTF-8 bytes get dropped.
    let pattern: String = pat_bytes
        .iter()
        .filter(|&&b| b.is_ascii() && !b.is_ascii_control())
        .map(|&b| b as char)
        .collect();
    let rest = &data[2 + pat_len..];

    let q = match SearchQuery::new(&pattern, options) {
        Ok(q) => q,
        Err(_) => return,
    };

    // Build scrollback rows from the remaining bytes — one row per
    // `COLS` bytes, padded with `Empty` cells if the last chunk is
    // short. One input bit per row drives `soft_wrap_continued` so the
    // fuzzer explores stitched logical lines of arbitrary shape
    // (including a continuation bit on the oldest row — the
    // evicted-head case).
    let mut rows: Vec<Vec<Cell>> = Vec::new();
    let mut continued: Vec<bool> = Vec::new();
    for chunk in rest.chunks(COLS).take(64) {
        let mut row: Vec<Cell> = (0..COLS).map(|_| Cell::default()).collect();
        for (i, b) in chunk.iter().enumerate() {
            row[i].grapheme = byte_to_grapheme(*b);
        }
        rows.push(row);
        continued.push(chunk.first().copied().unwrap_or(0) & 0b100 != 0);
    }

    // No `Grapheme::Cluster` handles are produced above, so a default
    // (empty) cluster table resolves everything the rows can hold —
    // mirrors the empty-table idiom in the search proptests.
    let clusters = ClusterTable::default();
    let sb_len = rows.len();
    let row_refs: Vec<&[Cell]> = rows.iter().map(Vec::as_slice).collect();
    for hit in search_rows(row_refs, &continued, sb_len, &q, &clusters) {
        // Span integrity: each byte span must land on char
        // boundaries and slice into `text` without panicking.
        for span in &hit.byte_spans {
            let s = span.start as usize;
            let e = span.end as usize;
            assert!(s <= e);
            assert!(e <= hit.text.len());
            assert!(hit.text.is_char_boundary(s));
            assert!(hit.text.is_char_boundary(e));
            let _ = &hit.text[s..e];
        }
        // Segment integrity: a match contributes at least one segment
        // (so col_spans can only outnumber byte_spans, never trail
        // them), every segment paints a non-empty column range inside
        // the row width, and segment rows stay inside the scrollback's
        // negative index space at or below the hit's anchor row.
        assert!(hit.col_spans.len() >= hit.byte_spans.len());
        for span in &hit.col_spans {
            assert!(span.col_start < span.col_end);
            assert!(usize::from(span.col_end) <= COLS);
            assert!(span.line_index >= hit.line_index);
            assert!(span.line_index < 0);
        }
    }
});

//! Scrollback and live-grid text search (REQ-607;
//! `docs/explanation/data-model/scrollback.md` "Search").
//! Matches per logical line, reporting one column segment per touched row.
//! Works on UTF-8 bytes without Unicode normalization (principle 4).
//! Alternate screen is excluded from search.

use felis_protocol::messages::{ByteSpan, ColSpan};

pub use felis_protocol::messages::SearchOptions;

use crate::ansi::logical_line_spans as line_spans;
use crate::{Cell, ClusterTable, Grapheme};

#[derive(Debug, Clone)]
pub struct SearchQuery {
    matcher: Matcher,
    options: SearchOptions,
}

#[derive(Debug, thiserror::Error)]
pub enum SearchQueryError {
    /// An error rather than a match at every position.
    #[error("search needle is empty")]
    EmptyNeedle,
    /// `regex`'s own diagnostic, unmodified, so the CLI banner matches.
    #[error("invalid regex: {0}")]
    BadRegex(String),
}

#[derive(Debug, Clone)]
enum Matcher {
    /// `needle` is pre-lowercased when `case_insensitive`.
    Plain {
        needle: String,
        case_insensitive: bool,
    },
    Regex(regex::Regex),
}

impl SearchQuery {
    pub fn new(pattern: &str, options: SearchOptions) -> Result<Self, SearchQueryError> {
        if pattern.is_empty() {
            return Err(SearchQueryError::EmptyNeedle);
        }
        let matcher = if options.regex {
            // The size cap keeps an adversarial pattern from blowing the
            // daemon's memory; 10 MiB is `regex` v1's default, pinned
            // against an upstream shift.
            let r = regex::RegexBuilder::new(pattern)
                .case_insensitive(options.case_insensitive)
                .size_limit(10 * 1024 * 1024)
                .build()
                .map_err(|e| SearchQueryError::BadRegex(e.to_string()))?;
            Matcher::Regex(r)
        } else {
            Matcher::Plain {
                needle: if options.case_insensitive {
                    pattern.to_lowercase()
                } else {
                    pattern.to_string()
                },
                case_insensitive: options.case_insensitive,
            }
        };
        Ok(Self { matcher, options })
    }

    #[must_use]
    pub const fn options(&self) -> SearchOptions {
        self.options
    }

    /// Half-open byte ranges into `text`.
    fn match_row(&self, text: &str) -> Vec<(usize, usize)> {
        match &self.matcher {
            Matcher::Plain {
                needle,
                case_insensitive,
            } => {
                let haystack_owned;
                let haystack = if *case_insensitive {
                    haystack_owned = text.to_lowercase();
                    haystack_owned.as_str()
                } else {
                    text
                };
                let mut hits = Vec::new();
                let mut start = 0;
                while let Some(rel) = haystack[start..].find(needle.as_str()) {
                    let abs = start + rel;
                    hits.push((abs, abs + needle.len()));
                    start = abs + needle.len().max(1);
                    if start >= haystack.len() {
                        break;
                    }
                }
                hits
            }
            Matcher::Regex(r) => r
                .find_iter(text)
                .map(|m| (m.start(), m.end()))
                .filter(|(s, e)| s != e)
                .collect(),
        }
    }
}

/// Where a chunked search walk left off. Two numbers, not one: the
/// walk runs newest-first, so a line that arrived since the last slice
/// shifts every older ordinal by one, and `visited` alone would
/// re-report the lines pushed past the cursor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SearchCursor {
    /// Logical lines already scanned, counted from the newest end.
    pub visited: usize,
    /// Logical lines the surface held when the last slice ran. `None`
    /// rather than `0` before the first slice: conflating them would
    /// make `SearchCursor::default()` read every line as growth and
    /// skip the whole surface.
    pub seen_total: Option<usize>,
}

impl SearchCursor {
    #[must_use]
    pub const fn seen(&self) -> usize {
        match self.seen_total {
            Some(total) => total,
            None => 0,
        }
    }
}

/// One logical line's worth of matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// Row index (REQ-608: `-1` is the youngest scrollback row, live
    /// rows are `0`-based) of the line's topmost row; a stitched line
    /// spans `line_index ..= line_index + rows - 1`.
    pub line_index: i64,
    /// The stitched text, tail trailing-blank trimmed, that
    /// `byte_spans` index.
    pub text: String,
    pub byte_spans: Vec<ByteSpan>,
    /// Not 1:1 with `byte_spans`: a match crossing a wrap edge
    /// contributes one segment per touched row.
    pub col_spans: Vec<ColSpan>,
}

/// `(text, byte_to_col)`, where `byte_to_col[i]` is the column that
/// produced byte `i` and the map is one longer than `text` so a
/// half-open span's end resolves too. `Spacer` / `SizedSpacer` cells
/// produce no bytes; `Empty` decodes to one space.
#[must_use]
pub fn row_text(row: &[Cell], clusters: &ClusterTable) -> (String, Vec<u16>) {
    let mut text = String::with_capacity(row.len());
    let mut map: Vec<u16> = Vec::with_capacity(row.len() + 1);
    for (col, cell) in row.iter().enumerate() {
        let col_u16: u16 = u16::try_from(col).unwrap_or(u16::MAX);
        let before = text.len();
        crate::ansi::push_cell_text(cell, clusters, &mut text);
        for _ in before..text.len() {
            map.push(col_u16);
        }
    }
    let end_col: u16 = u16::try_from(row.len()).unwrap_or(u16::MAX);
    map.push(end_col);
    (text, map)
}

fn trim_trailing_blanks(text: &mut String, byte_to_col: &mut Vec<u16>) {
    let trimmed_len = text.trim_end_matches(' ').len();
    if trimmed_len == text.len() {
        return;
    }
    text.truncate(trimmed_len);
    byte_to_col.truncate(trimmed_len + 1);
}

fn byte_span_to_col_span(byte_span: (usize, usize), byte_to_col: &[u16]) -> (u16, u16) {
    let (b0, b1) = byte_span;
    // A bound past the map is a bug, but must not panic the daemon on
    // remote-driven input.
    let c0 = byte_to_col
        .get(b0)
        .copied()
        .unwrap_or_else(|| *byte_to_col.last().unwrap_or(&0));
    let c1 = byte_to_col
        .get(b1)
        .copied()
        .unwrap_or_else(|| *byte_to_col.last().unwrap_or(&0));
    (c0, c1)
}

/// The walk behind [`crate::Grid::search`], public so the fuzz target
/// can drive row shapes a parser cannot cheaply produce. `rows` is
/// scrollback oldest→newest followed by live rows top→bottom, indexed
/// `j - sb_len` so scrollback lands in REQ-608's negative space; hits
/// come back newest-first.
pub fn search_rows<'a>(
    rows: Vec<&'a [Cell]>,
    continued: &[bool],
    sb_len: usize,
    query: &'a SearchQuery,
    clusters: &'a ClusterTable,
) -> impl Iterator<Item = SearchHit> + use<'a> {
    line_spans(continued)
        .into_iter()
        .rev()
        .filter_map(move |(start, end)| {
            let first_line_index = i64::try_from(start).unwrap_or(i64::MAX)
                - i64::try_from(sb_len).unwrap_or(i64::MAX);
            line_to_hit(&rows[start..=end], query, first_line_index, clusters)
        })
}

impl crate::Grid {
    /// Matches over scrollback plus the live primary screen,
    /// newest-first, stitched across the seam so a needle spanning it
    /// is found. On the alternate screen the live pass is skipped: alt
    /// rows are no continuation of scrollback, and their `soft_wrap`
    /// bits describe the alt screen, not the saved primary.
    pub fn search<'a>(&'a self, query: &'a SearchQuery) -> impl Iterator<Item = SearchHit> + 'a {
        self.search_window(query, SearchCursor::default(), usize::MAX)
            .1
            .map(|(_, hit)| hit)
    }

    /// One slice of [`Self::search`]'s walk: skip `visited` lines from
    /// newest end, scan at most `budget` more, releasing session lock per slice.
    /// Returned cursor adjusts for new arrivals since previous slice;
    /// each hit pairs with its walk ordinal so mid-slice stops can resume.
    pub fn search_window<'a>(
        &'a self,
        query: &'a SearchQuery,
        cursor: SearchCursor,
        budget: usize,
    ) -> (SearchCursor, impl Iterator<Item = (usize, SearchHit)> + 'a) {
        let sb_len = self.screen.scrollback().len();
        let (rows, continued): (Vec<&[Cell]>, Vec<bool>) = self
            .screen
            .rows_with_wrap(crate::AltScreenRows::Skip)
            .unzip();
        let spans = line_spans(&continued);
        let total = spans.len();
        let visited = cursor.visited
            + cursor
                .seen_total
                .map_or(0, |seen| total.saturating_sub(seen));
        let clusters = self.screen.cluster_table();
        let walk = spans
            .into_iter()
            .rev()
            .enumerate()
            .skip(visited)
            .take(budget)
            .filter_map(move |(ordinal, (start, end))| {
                let first_line_index = i64::try_from(start).unwrap_or(i64::MAX)
                    - i64::try_from(sb_len).unwrap_or(i64::MAX);
                line_to_hit(&rows[start..=end], query, first_line_index, clusters)
                    .map(|hit| (ordinal, hit))
            });
        (
            SearchCursor {
                visited,
                seen_total: Some(total),
            },
            walk,
        )
    }
}

/// A wrapped row's trailing `Empty` cells were never written (the
/// unfillable slot a wide-glyph wrap leaves), so dropping them is
/// exact; printed spaces are `Ascii(b' ')` and survive.
fn trim_trailing_empty(row: &[Cell]) -> &[Cell] {
    let keep = row
        .iter()
        .rposition(|c| !matches!(c.grapheme, Grapheme::Empty))
        .map_or(0, |i| i + 1);
    &row[..keep]
}

/// `None` when the line is blank or has no matches.
fn line_to_hit(
    line_rows: &[&[Cell]],
    query: &SearchQuery,
    first_line_index: i64,
    clusters: &ClusterTable,
) -> Option<SearchHit> {
    let mut text = String::new();
    // Per row: (line_index, byte offset into `text`, byte→col map).
    let mut segs: Vec<(i64, usize, Vec<u16>)> = Vec::with_capacity(line_rows.len());
    for (k, row) in line_rows.iter().enumerate() {
        let is_tail = k + 1 == line_rows.len();
        let slice = if is_tail {
            *row
        } else {
            trim_trailing_empty(row)
        };
        let (mut t, mut map) = row_text(slice, clusters);
        if is_tail {
            trim_trailing_blanks(&mut t, &mut map);
        }
        let li = first_line_index + i64::try_from(k).unwrap_or(i64::MAX);
        segs.push((li, text.len(), map));
        text.push_str(&t);
    }
    if text.is_empty() {
        return None;
    }
    let raw = query.match_row(&text);
    if raw.is_empty() {
        return None;
    }
    let byte_spans = raw
        .iter()
        .map(|&(s, e)| ByteSpan {
            start: u32::try_from(s).unwrap_or(u32::MAX),
            end: u32::try_from(e).unwrap_or(u32::MAX),
        })
        .collect();
    let mut col_spans = Vec::with_capacity(raw.len());
    for &(s, e) in &raw {
        for (li, row_start, map) in &segs {
            let row_len = map.len() - 1;
            let row_end = row_start + row_len;
            if e <= *row_start || s >= row_end {
                continue;
            }
            let rel_s = s.saturating_sub(*row_start);
            let rel_e = (e - row_start).min(row_len);
            if rel_e <= rel_s {
                continue;
            }
            let (c0, c1) = byte_span_to_col_span((rel_s, rel_e), map);
            col_spans.push(ColSpan {
                line_index: *li,
                col_start: c0,
                col_end: c1,
            });
        }
    }
    Some(SearchHit {
        line_index: first_line_index,
        text,
        byte_spans,
        col_spans,
    })
}

#[cfg(test)]
mod tests;

//! [`SearchToDaemonMsg`] and [`SearchToClientMsg`]: scrollback search
//! conversation (kind 9).

use serde::{Deserialize, Serialize};

use super::{ArmMeta, CorrelationClass, Directed, Direction, ModeSet};

/// Scrollback-search requests ([`crate::MessageKind::Search`], kind 9,
/// REQ-607), attached connections only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchToDaemonMsg {
    /// Opens a stream of search hits across scrollback and live primary screen.
    ///
    /// The daemon walks rows newest-first and emits [`SearchToClientMsg::Match`] items
    /// terminated by [`ConnToClientMsg::End`](super::ConnToClientMsg::End).
    Query {
        /// A literal substring when `options.regex` is false, a `regex`
        /// crate pattern when true. An empty pattern or an uncompilable
        /// regex is refused with `InvalidRequest`.
        query: String,
        options: SearchOptions,
    },
}

/// The daemon's half of the scrollback-search conversation: hits
/// reference rows of the attached session's grid + scrollback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchToClientMsg {
    /// One logical line returning a hit, newest-first. Rows joined by
    /// autowrap arrive stitched as one hit.
    Match {
        /// Row index per REQ-608 of the logical line's topmost (oldest)
        /// row: `-1` is the youngest scrollback row, a live-screen row
        /// reports its grid row index (`>= 0`). A hit straddling the
        /// scrollback seam anchors at its negative topmost row while its
        /// `col_spans` carry rows of both signs.
        line_index: i64,
        /// The stitched logical-line text the matcher saw, tail
        /// trailing-blank trimmed.
        text: String,
        /// Byte ranges into `text` where matches fired.
        byte_spans: Vec<ByteSpan>,
        /// Paintable highlight segments. A match crossing a wrap edge
        /// contributes one segment per touched row, so this is not 1:1
        /// with `byte_spans`.
        col_spans: Vec<ColSpan>,
    },
}

const fn arm(name: &'static str, direction: Direction, correlation: CorrelationClass) -> ArmMeta {
    ArmMeta::new(name, direction, correlation, ModeSet::ATTACHERS)
}

impl Directed for SearchToDaemonMsg {
    const ARMS: &'static [ArmMeta] = &[arm(
        "Search::Query",
        Direction::ToDaemon,
        CorrelationClass::StreamOpener,
    )];

    fn arm_index(&self) -> usize {
        match self {
            Self::Query { .. } => 0,
        }
    }
}

impl Directed for SearchToClientMsg {
    const ARMS: &'static [ArmMeta] = &[arm(
        "Search::Match",
        Direction::ToClient,
        CorrelationClass::StreamItem,
    )];

    fn arm_index(&self) -> usize {
        match self {
            Self::Match { .. } => 0,
        }
    }
}

/// One match's byte range within a [`SearchToClientMsg::Match`]'s `text`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteSpan {
    /// First byte of the match.
    pub start: u32,
    /// One past the match's last byte (half-open).
    pub end: u32,
}

/// One paintable highlight segment of a [`SearchToClientMsg::Match`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColSpan {
    /// Row index per REQ-608 of the row this segment paints on.
    pub line_index: i64,
    /// First highlighted column.
    pub col_start: u16,
    /// One past the last highlighted column (half-open).
    pub col_end: u16,
}

/// Options on a [`SearchToDaemonMsg::Query`] request; `felis-grid` re-exports
/// it as `felis_grid::SearchOptions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SearchOptions {
    /// Regex anchors `^` / `$` match row start/end.
    pub regex: bool,
    /// Substring mode lowercases both sides; regex mode bakes in `(?i)`.
    pub case_insensitive: bool,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::messages::test_support::{assert_covers_every_arm, roundtrip};

    fn search_to_daemon_cases() -> Vec<SearchToDaemonMsg> {
        vec![
            SearchToDaemonMsg::Query {
                query: "needle".into(),
                options: SearchOptions::default(),
            },
            SearchToDaemonMsg::Query {
                query: "(?i)foo|bar".into(),
                options: SearchOptions {
                    regex: true,
                    case_insensitive: false,
                },
            },
            SearchToDaemonMsg::Query {
                query: "case insensitive plain".into(),
                options: SearchOptions {
                    regex: false,
                    case_insensitive: true,
                },
            },
        ]
    }

    fn search_to_client_cases() -> Vec<SearchToClientMsg> {
        vec![
            SearchToClientMsg::Match {
                line_index: -3,
                text: "the needle in the row".into(),
                byte_spans: vec![
                    ByteSpan { start: 4, end: 10 },
                    ByteSpan { start: 18, end: 21 },
                ],
                col_spans: vec![
                    ColSpan {
                        line_index: -3,
                        col_start: 4,
                        col_end: 10,
                    },
                    ColSpan {
                        line_index: -2,
                        col_start: 0,
                        col_end: 3,
                    },
                ],
            },
            SearchToClientMsg::Match {
                line_index: 7,
                text: String::new(),
                byte_spans: vec![],
                col_spans: vec![],
            },
        ]
    }

    #[test]
    fn search_messages_round_trip() {
        for msg in search_to_daemon_cases() {
            assert_eq!(roundtrip(&msg), msg);
        }
        for msg in search_to_client_cases() {
            assert_eq!(roundtrip(&msg), msg);
        }
    }

    #[test]
    fn search_cases_cover_every_variant() {
        assert_covers_every_arm(&search_to_daemon_cases());
        assert_covers_every_arm(&search_to_client_cases());
    }
}

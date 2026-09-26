//! [`RegionToDaemonMsg`] and [`RegionToClientMsg`]: region-export family
//! (kind 6).

use serde::{Deserialize, Serialize};

use super::{ArmMeta, CorrelationClass, Directed, Direction, ModeSet, RegionSource};

/// Region-export requests ([`crate::MessageKind::Region`], kind 6),
/// sent while attached.
///
/// The daemon serializes regions while the requester retains sink routing
/// (`docs/explanation/input.md` "Which daemon runs the command").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RegionToDaemonMsg {
    /// Serialize a region of the attached session's
    /// buffer and reply with a single stitched [`RegionToClientMsg::Reply`]
    /// (`docs/explanation/data-model/scrollback.md` "Piping to an
    /// external command").
    Request {
        source: RegionSource,
        /// Reconstruct SGR color/style, or emit plain text. Mirrors
        /// `felis sessions capture --ansi` (default off).
        ansi: bool,
    },
    /// Serialize the same region [`Self::Request`]
    /// names, but reply row by row ([`RegionToClientMsg::Row`] … [`RegionToClientMsg::RowsDone`]).
    /// Behind `felis sessions capture`.
    Rows {
        source: RegionSource,
        /// Also carry each row's SGR reconstruction ([`RegionToClientMsg::Row::ansi`]);
        /// the plain text always ships.
        ansi: bool,
        /// Emit only the youngest `max_rows` rows (still oldest-first);
        /// `None` emits the whole region.
        max_rows: Option<u32>,
    },
}

/// The daemon's half of the region-export family: the one-shot reply
/// and the row stream's items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RegionToClientMsg {
    /// Reply to [`RegionToDaemonMsg::Request`], sent to requester only.
    ///
    /// Sinks are retained locally by the requester rather than echoed across IPC.
    Reply {
        /// Empty when the region was absent or empty.
        data: Vec<u8>,
        /// `None` for a region with no viewport anchor (the `OSC 133`
        /// mark sources).
        position: Option<RegionPosition>,
        /// Exit code carried by the youngest `OSC 133 ; D` mark; `None`
        /// when no region resolved or no `D` mark carried a code.
        exit_code: Option<u32>,
    },
    /// One region row, oldest-first.
    Row {
        /// The row's index in the region's own coordinate space:
        /// scrollback rows are negative (`-1` the youngest retained
        /// row), live grid rows count `0..rows`, and a mark-range row
        /// counts from `0` at the range's first row. A `max_rows` tail
        /// keeps the untrimmed indices.
        row: i32,
        /// Trailing blanks trimmed.
        text: String,
        /// The row with reconstructed SGR runs, when the request set
        /// `ansi`.
        ansi: Option<String>,
        /// Ties this row to the one above as a single logical line
        /// (`felis_grid::logical_lines`).
        soft_wrap_continued: bool,
    },
    /// The last item of a [`RegionToDaemonMsg::Rows`] stream, after
    /// the last [`Self::Row`]. Not the terminator: the stream is closed
    /// by the shared [`ConnToClientMsg::End`](super::ConnToClientMsg::End).
    RowsDone {
        /// Same contract as [`Self::Reply::exit_code`].
        exit_code: Option<u32>,
    },
}

const fn arm(name: &'static str, direction: Direction, correlation: CorrelationClass) -> ArmMeta {
    ArmMeta::new(name, direction, correlation, ModeSet::ATTACHERS)
}

impl Directed for RegionToDaemonMsg {
    const ARMS: &'static [ArmMeta] = &[
        arm(
            "Region::Request",
            Direction::ToDaemon,
            CorrelationClass::RequestOpener,
        ),
        arm(
            "Region::Rows",
            Direction::ToDaemon,
            CorrelationClass::StreamOpener,
        ),
    ];

    fn arm_index(&self) -> usize {
        match self {
            Self::Request { .. } => 0,
            Self::Rows { .. } => 1,
        }
    }
}

impl Directed for RegionToClientMsg {
    const ARMS: &'static [ArmMeta] = &[
        arm(
            "Region::Reply",
            Direction::ToClient,
            CorrelationClass::RequestReply,
        ),
        arm(
            "Region::Row",
            Direction::ToClient,
            CorrelationClass::StreamItem,
        ),
        arm(
            "Region::RowsDone",
            Direction::ToClient,
            CorrelationClass::StreamItem,
        ),
    ];

    fn arm_index(&self) -> usize {
        match self {
            Self::Reply { .. } => 0,
            Self::Row { .. } => 1,
            Self::RowsDone { .. } => 2,
        }
    }
}

/// The user's place inside a serialized region: 1-based coordinates in
/// the region's stitched logical-line space (the child's
/// `FELIS_INPUT_LINE_NUMBER` / `FELIS_CURSOR_LINE` /
/// `FELIS_CURSOR_COLUMN` and the pager's `+N`). Columns count rendered
/// characters, so an `ansi` region does not shift them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionPosition {
    /// Logical line holding the window's top row.
    pub top_line: u32,
    /// Logical line holding the cursor.
    pub cursor_line: u32,
    /// Cursor column within that line.
    pub cursor_column: u32,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::messages::test_support::{assert_covers_every_arm, roundtrip};

    fn region_to_daemon_cases() -> Vec<RegionToDaemonMsg> {
        vec![
            RegionToDaemonMsg::Request {
                source: RegionSource::Scrollback,
                ansi: true,
            },
            RegionToDaemonMsg::Request {
                source: RegionSource::Visible,
                ansi: false,
            },
            RegionToDaemonMsg::Request {
                source: RegionSource::LastCommand,
                ansi: false,
            },
            RegionToDaemonMsg::Request {
                source: RegionSource::CommandOutput,
                ansi: true,
            },
            RegionToDaemonMsg::Rows {
                source: RegionSource::Scrollback,
                ansi: false,
                max_rows: None,
            },
            RegionToDaemonMsg::Rows {
                source: RegionSource::CommandOutput,
                ansi: true,
                max_rows: Some(200),
            },
        ]
    }

    fn region_to_client_cases() -> Vec<RegionToClientMsg> {
        vec![
            RegionToClientMsg::Reply {
                data: b"line one\nline two\n".to_vec(),
                position: Some(RegionPosition {
                    top_line: 120,
                    cursor_line: 121,
                    cursor_column: 7,
                }),
                exit_code: None,
            },
            RegionToClientMsg::Reply {
                data: b"command output\n".to_vec(),
                position: None,
                exit_code: Some(1),
            },
            RegionToClientMsg::Reply {
                data: Vec::new(),
                position: None,
                exit_code: None,
            },
            RegionToClientMsg::Row {
                row: -3,
                text: "cargo build".into(),
                ansi: None,
                soft_wrap_continued: false,
            },
            RegionToClientMsg::Row {
                row: 0,
                text: "error[E0308]".into(),
                ansi: Some("\u{1b}[31merror[E0308]\u{1b}[0m".into()),
                soft_wrap_continued: true,
            },
            RegionToClientMsg::RowsDone { exit_code: Some(1) },
            RegionToClientMsg::RowsDone { exit_code: None },
        ]
    }

    #[test]
    fn region_messages_round_trip() {
        for msg in region_to_daemon_cases() {
            assert_eq!(roundtrip(&msg), msg);
        }
        for msg in region_to_client_cases() {
            assert_eq!(roundtrip(&msg), msg);
        }
    }

    #[test]
    fn region_cases_cover_every_variant() {
        assert_covers_every_arm(&region_to_daemon_cases());
        assert_covers_every_arm(&region_to_client_cases());
    }
}

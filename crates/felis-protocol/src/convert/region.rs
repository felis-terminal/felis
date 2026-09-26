//! `RegionToDaemonMsg` / `RegionToClientMsg` <-> wire.

use super::{WireError, region_source_from_i32, region_source_to_i32};
use crate::messages;
use crate::wire::v1;

impl From<&messages::RegionToDaemonMsg> for v1::RegionToDaemonMsg {
    fn from(m: &messages::RegionToDaemonMsg) -> Self {
        use messages::RegionToDaemonMsg as R;
        use v1::region_to_daemon_msg::Msg;
        let msg = match m {
            R::Request { source, ansi } => Msg::Request(v1::RegionRequest {
                source: region_source_to_i32(*source),
                ansi: *ansi,
            }),
            R::Rows {
                source,
                ansi,
                max_rows,
            } => Msg::Rows(v1::RegionRows {
                source: region_source_to_i32(*source),
                ansi: *ansi,
                max_rows: *max_rows,
            }),
        };
        Self {
            msg: Some(msg),
            correlation: None,
        }
    }
}

impl From<&messages::RegionToClientMsg> for v1::RegionToClientMsg {
    fn from(m: &messages::RegionToClientMsg) -> Self {
        use messages::RegionToClientMsg as R;
        use v1::region_to_client_msg::Msg;
        let msg = match m {
            R::Reply {
                data,
                position,
                exit_code,
            } => Msg::Reply(v1::RegionReply {
                data: data.clone(),
                position: position.map(Into::into),
                exit_code: *exit_code,
            }),
            R::Row {
                row,
                text,
                ansi,
                soft_wrap_continued,
            } => Msg::Row(v1::RegionRow {
                row: *row,
                text: text.clone(),
                ansi: ansi.clone(),
                soft_wrap_continued: *soft_wrap_continued,
            }),
            R::RowsDone { exit_code } => Msg::RowsDone(v1::RegionRowsDone {
                exit_code: *exit_code,
            }),
        };
        Self {
            msg: Some(msg),
            correlation: None,
        }
    }
}

impl From<messages::RegionPosition> for v1::RegionPosition {
    fn from(p: messages::RegionPosition) -> Self {
        Self {
            top_line: p.top_line,
            cursor_line: p.cursor_line,
            cursor_column: p.cursor_column,
        }
    }
}

impl From<v1::RegionPosition> for messages::RegionPosition {
    fn from(p: v1::RegionPosition) -> Self {
        Self {
            top_line: p.top_line,
            cursor_line: p.cursor_line,
            cursor_column: p.cursor_column,
        }
    }
}

impl TryFrom<v1::RegionToDaemonMsg> for messages::RegionToDaemonMsg {
    type Error = WireError;
    fn try_from(m: v1::RegionToDaemonMsg) -> Result<Self, Self::Error> {
        use v1::region_to_daemon_msg::Msg;
        Ok(
            match m
                .msg
                .ok_or(WireError::MissingOneof("RegionToDaemonMsg.msg"))?
            {
                Msg::Request(r) => Self::Request {
                    source: region_source_from_i32(r.source)?,
                    ansi: r.ansi,
                },
                Msg::Rows(r) => Self::Rows {
                    source: region_source_from_i32(r.source)?,
                    ansi: r.ansi,
                    max_rows: r.max_rows,
                },
            },
        )
    }
}

impl TryFrom<v1::RegionToClientMsg> for messages::RegionToClientMsg {
    type Error = WireError;
    fn try_from(m: v1::RegionToClientMsg) -> Result<Self, Self::Error> {
        use v1::region_to_client_msg::Msg;
        Ok(
            match m
                .msg
                .ok_or(WireError::MissingOneof("RegionToClientMsg.msg"))?
            {
                Msg::Reply(r) => Self::Reply {
                    data: r.data,
                    position: r.position.map(Into::into),
                    exit_code: r.exit_code,
                },
                Msg::Row(r) => Self::Row {
                    row: r.row,
                    text: r.text,
                    ansi: r.ansi,
                    soft_wrap_continued: r.soft_wrap_continued,
                },
                Msg::RowsDone(r) => Self::RowsDone {
                    exit_code: r.exit_code,
                },
            },
        )
    }
}

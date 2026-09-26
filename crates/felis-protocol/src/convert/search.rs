//! `SearchToDaemonMsg` / `SearchToClientMsg` <-> wire, with the byte/column span
//! helpers.

use super::{WireError, narrow};
use crate::messages;
use crate::wire::v1;

const fn byte_span_to_wire(s: messages::ByteSpan) -> v1::ByteSpan {
    v1::ByteSpan {
        start: s.start,
        end: s.end,
    }
}

const fn byte_span_from_wire(s: v1::ByteSpan) -> messages::ByteSpan {
    messages::ByteSpan {
        start: s.start,
        end: s.end,
    }
}

fn col_span_to_wire(c: messages::ColSpan) -> v1::ColSpan {
    v1::ColSpan {
        line_index: c.line_index,
        col_start: u32::from(c.col_start),
        col_end: u32::from(c.col_end),
    }
}

fn col_span_from_wire(c: v1::ColSpan) -> Result<messages::ColSpan, WireError> {
    Ok(messages::ColSpan {
        line_index: c.line_index,
        col_start: narrow("ColSpan.col_start", c.col_start)?,
        col_end: narrow("ColSpan.col_end", c.col_end)?,
    })
}

impl From<messages::SearchOptions> for v1::SearchOptions {
    fn from(o: messages::SearchOptions) -> Self {
        Self {
            regex: o.regex,
            case_insensitive: o.case_insensitive,
        }
    }
}

impl From<v1::SearchOptions> for messages::SearchOptions {
    fn from(o: v1::SearchOptions) -> Self {
        Self {
            regex: o.regex,
            case_insensitive: o.case_insensitive,
        }
    }
}

fn search_options_from_wire(
    options: Option<v1::SearchOptions>,
) -> Result<messages::SearchOptions, WireError> {
    Ok(options
        .ok_or(WireError::MissingField("SearchQuery.options"))?
        .into())
}

impl From<&messages::SearchToDaemonMsg> for v1::SearchToDaemonMsg {
    fn from(m: &messages::SearchToDaemonMsg) -> Self {
        use messages::SearchToDaemonMsg as S;
        use v1::search_to_daemon_msg::Msg;
        let msg = match m {
            S::Query { query, options } => Msg::Query(v1::SearchQuery {
                query: query.clone(),
                options: Some((*options).into()),
            }),
        };
        Self {
            msg: Some(msg),
            correlation: None,
        }
    }
}

impl From<&messages::SearchToClientMsg> for v1::SearchToClientMsg {
    fn from(m: &messages::SearchToClientMsg) -> Self {
        use messages::SearchToClientMsg as S;
        use v1::search_to_client_msg::Msg;
        let msg = match m {
            S::Match {
                line_index,
                text,
                byte_spans,
                col_spans,
            } => Msg::Match(v1::SearchMatch {
                line_index: *line_index,
                text: text.clone(),
                byte_spans: byte_spans.iter().copied().map(byte_span_to_wire).collect(),
                col_spans: col_spans.iter().copied().map(col_span_to_wire).collect(),
            }),
        };
        Self {
            msg: Some(msg),
            correlation: None,
        }
    }
}

impl TryFrom<v1::SearchToDaemonMsg> for messages::SearchToDaemonMsg {
    type Error = WireError;
    fn try_from(m: v1::SearchToDaemonMsg) -> Result<Self, Self::Error> {
        use v1::search_to_daemon_msg::Msg;
        Ok(
            match m
                .msg
                .ok_or(WireError::MissingOneof("SearchToDaemonMsg.msg"))?
            {
                Msg::Query(s) => Self::Query {
                    query: s.query,
                    options: search_options_from_wire(s.options)?,
                },
            },
        )
    }
}

impl TryFrom<v1::SearchToClientMsg> for messages::SearchToClientMsg {
    type Error = WireError;
    fn try_from(m: v1::SearchToClientMsg) -> Result<Self, Self::Error> {
        use v1::search_to_client_msg::Msg;
        Ok(
            match m
                .msg
                .ok_or(WireError::MissingOneof("SearchToClientMsg.msg"))?
            {
                Msg::Match(s) => Self::Match {
                    line_index: s.line_index,
                    text: s.text,
                    byte_spans: s.byte_spans.into_iter().map(byte_span_from_wire).collect(),
                    col_spans: s
                        .col_spans
                        .into_iter()
                        .map(col_span_from_wire)
                        .collect::<Result<_, _>>()?,
                },
            },
        )
    }
}

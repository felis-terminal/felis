//! OSC 7501 Program Status Protocol decoder (decode-only), spec revision 0.4:
//! <https://www.superlogical.com/rex/docs/build/program-status>.
//!
//! Every check finishes before anything is built, so a dropped report
//! carries nothing a caller could half-apply.

use crate::split_osc_first;
use felis_protocol::base64;

/// The spec's per-report caps, plus the per-terminal record cap.
pub mod limits {
    /// Whole sequence, `ESC ]` through the terminator.
    pub const SEQUENCE_MAX: usize = 4096;
    pub const KEY_MAX: usize = 16;
    pub const MSG_ENCODED_MAX: usize = 2732;
    pub const MSG_DECODED_MAX: usize = 2048;
    pub const TITLE_ENCODED_MAX: usize = 256;
    pub const TITLE_DECODED_MAX: usize = 192;
    pub const APP_MAX: usize = 32;
    pub const ID_MAX: usize = 128;
    pub const ID_SEGMENT_MAX: usize = 32;
    pub const ID_DEPTH_MAX: usize = 8;
    /// Past this, the least recently updated record makes room.
    pub const RECORDS_MAX: usize = 256;
}

use limits::{
    APP_MAX, ID_DEPTH_MAX, ID_MAX, ID_SEGMENT_MAX, KEY_MAX, MSG_DECODED_MAX, MSG_ENCODED_MAX,
    SEQUENCE_MAX, TITLE_DECODED_MAX, TITLE_ENCODED_MAX,
};

/// felis never reads 8-bit C1, so the introducer is always `ESC ]`.
const INTRODUCER_LEN: usize = 2;

/// How a sequence ended. An OSC interrupted by an ESC that does not start
/// ST has no terminator and must not reach [`parse_osc7501`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminator {
    Bel,
    St,
}

impl Terminator {
    const fn len(self) -> usize {
        match self {
            Self::Bel => 1,
            Self::St => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "state-dump", derive(serde::Serialize, serde::Deserialize))]
pub enum State {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
}

impl State {
    pub const ALL: [Self; 5] = [
        Self::Idle,
        Self::Working,
        Self::Done,
        Self::Blocked,
        Self::Error,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Error => "error",
        }
    }

    fn from_wire(value: &[u8]) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|s| s.as_str().as_bytes() == value)
    }

    const fn carries_progress(self) -> bool {
        matches!(self, Self::Working | Self::Blocked)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "state-dump", derive(serde::Serialize, serde::Deserialize))]
pub enum Kind {
    Permission,
    Question,
    Auth,
}

impl Kind {
    pub const ALL: [Self; 3] = [Self::Permission, Self::Question, Self::Auth];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::Question => "question",
            Self::Auth => "auth",
        }
    }

    fn from_wire(value: &[u8]) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|k| k.as_str().as_bytes() == value)
    }
}

/// One accepted report: the whole content of the record it replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "state-dump", derive(serde::Serialize, serde::Deserialize))]
pub struct Report {
    /// `""` is the root record.
    pub id: String,
    pub state: State,
    /// Only with [`State::Blocked`].
    pub kind: Option<Kind>,
    /// `0..=100`, only with [`State::Working`] or [`State::Blocked`].
    pub progress: Option<u8>,
    /// As sent; inheriting it from an ancestor is the consumer's job.
    pub app: Option<String>,
    /// Decoded and free of control characters; bidi overrides are kept,
    /// since disarming them belongs to whoever displays the text.
    pub title: Option<String>,
    pub msg: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Id,
    App,
    Title,
    Msg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    TooLong,
    KeyTooLong,
    FieldTooLong(Field),
    BadBase64(Field),
    NotUtf8(Field),
    ControlChar(Field),
    MissingState,
    UnknownState,
    BadId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Osc7501 {
    /// `OSC 7501 ; ? ST`; the caller writes the reply.
    Query,
    Report(Report),
    /// `""` removes every record; otherwise the record and its descendants.
    Clear {
        id: String,
    },
    /// Nothing from the sequence may be applied.
    Dropped(DropReason),
}

/// `None` only if the code is not `7501`.
#[must_use]
pub fn parse_osc7501(body: &[u8], terminator: Terminator) -> Option<Osc7501> {
    let (code, payload) = split_osc_first(body);
    if code != b"7501" {
        return None;
    }
    let wire_len = INTRODUCER_LEN + body.len() + terminator.len();
    Some(decode(payload, wire_len).unwrap_or_else(Osc7501::Dropped))
}

#[derive(Default)]
struct Fields<'a> {
    state: Option<&'a [u8]>,
    id: Option<&'a [u8]>,
    kind: Option<&'a [u8]>,
    progress: Option<&'a [u8]>,
    app: Option<&'a [u8]>,
    title: Option<String>,
    msg: Option<String>,
}

fn decode(payload: Option<&[u8]>, wire_len: usize) -> Result<Osc7501, DropReason> {
    if wire_len > SEQUENCE_MAX {
        return Err(DropReason::TooLong);
    }
    let Some(payload) = payload else {
        return Err(DropReason::MissingState);
    };
    if payload.trim_ascii() == b"?" {
        return Ok(Osc7501::Query);
    }

    // Hard checks run on every occurrence, before last-wins can discard an
    // earlier one that broke a limit.
    let mut f = Fields::default();
    for pair in payload.split(|&b| b == b':') {
        let Some(eq) = pair.iter().position(|&b| b == b'=') else {
            continue;
        };
        let key = pair[..eq].trim_ascii();
        let value = pair[eq + 1..].trim_ascii();
        if key.len() > KEY_MAX {
            return Err(DropReason::KeyTooLong);
        }
        if !value.iter().copied().all(is_value_byte) {
            // Skipping a malformed id would route the report to the root.
            if key == b"id" {
                return Err(DropReason::BadId);
            }
            continue;
        }
        match key {
            b"state" => f.state = Some(value),
            b"id" => f.id = Some(check_id(value)?),
            b"kind" => f.kind = Some(value),
            b"progress" => f.progress = Some(value),
            b"app" => {
                if value.len() > APP_MAX {
                    return Err(DropReason::FieldTooLong(Field::App));
                }
                f.app = Some(value);
            }
            b"title" => {
                f.title = Some(decode_text(
                    value,
                    Field::Title,
                    TITLE_ENCODED_MAX,
                    TITLE_DECODED_MAX,
                )?);
            }
            b"msg" => {
                f.msg = Some(decode_text(
                    value,
                    Field::Msg,
                    MSG_ENCODED_MAX,
                    MSG_DECODED_MAX,
                )?);
            }
            _ => {}
        }
    }

    let id = f.id.map(ascii_string).unwrap_or_default();
    let state = match f.state {
        None => return Err(DropReason::MissingState),
        Some(b"clear") => return Ok(Osc7501::Clear { id }),
        Some(value) => State::from_wire(value).ok_or(DropReason::UnknownState)?,
    };
    Ok(Osc7501::Report(Report {
        id,
        state,
        kind: f
            .kind
            .filter(|_| state == State::Blocked)
            .and_then(Kind::from_wire),
        progress: f
            .progress
            .filter(|_| state.carries_progress())
            .and_then(parse_progress),
        app: f.app.filter(|a| is_app(a)).map(ascii_string),
        title: f.title,
        msg: f.msg,
    }))
}

const fn is_value_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b',' | b'+' | b'/' | b'=' | b'-')
}

const fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-')
}

fn is_app(value: &[u8]) -> bool {
    !value.is_empty() && value.iter().copied().all(is_name_byte)
}

fn check_id(value: &[u8]) -> Result<&[u8], DropReason> {
    if value.len() > ID_MAX {
        return Err(DropReason::FieldTooLong(Field::Id));
    }
    let mut depth = 0;
    for segment in value.split(|&b| b == b'/') {
        depth += 1;
        if depth > ID_DEPTH_MAX || segment.len() > ID_SEGMENT_MAX {
            return Err(DropReason::FieldTooLong(Field::Id));
        }
        if segment.is_empty() || !segment.iter().copied().all(is_name_byte) {
            return Err(DropReason::BadId);
        }
    }
    Ok(value)
}

fn parse_progress(value: &[u8]) -> Option<u8> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let n = value.iter().fold(0u32, |n, &d| {
        n.saturating_mul(10).saturating_add(u32::from(d - b'0'))
    });
    u8::try_from(n).ok().filter(|&p| p <= 100)
}

fn decode_text(
    value: &[u8],
    field: Field,
    encoded_max: usize,
    decoded_max: usize,
) -> Result<String, DropReason> {
    if value.len() > encoded_max {
        return Err(DropReason::FieldTooLong(field));
    }
    let bytes = base64::decode(value).ok_or(DropReason::BadBase64(field))?;
    if bytes.len() > decoded_max {
        return Err(DropReason::FieldTooLong(field));
    }
    let text = String::from_utf8(bytes).map_err(|_| DropReason::NotUtf8(field))?;
    if text.chars().any(char::is_control) {
        return Err(DropReason::ControlChar(field));
    }
    Ok(text)
}

/// Callers pass bytes already checked against an ASCII charset.
fn ascii_string(value: &[u8]) -> String {
    value.iter().map(|&b| char::from(b)).collect()
}

#[cfg(test)]
mod tests;

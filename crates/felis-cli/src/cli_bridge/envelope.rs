use felis_client_core::Reconnector;
use felis_protocol::messages::StreamErrorReason;
use serde_json::Value;

use super::{link::LinkError, stdio::OutputClosed};
use crate::cli_output::{ErrorKind, SURFACE_VERSION};

/// The vocabulary is [`ErrorKind`], the same closed set the one-shot
/// verbs carry, so an editor and a script branch on one spelling per
/// failure (docs/reference/cli.md "Machine output").
#[derive(Debug, Clone)]
pub(super) struct BridgeError {
    pub(super) kind: ErrorKind,
    pub(super) message: String,
}

impl BridgeError {
    pub(super) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(super) fn malformed(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::MalformedRequest, message)
    }

    /// A well-formed request refused on its merits.
    pub(super) fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidRequest, message)
    }

    /// A payload past its documented per-operation limit (REQ-105a).
    /// Checked here rather than left to the writer so the caller reads
    /// a parseable error object on its request id instead of losing the
    /// connection under it.
    pub(super) fn over_limit(err: &felis_protocol::convert::WireError) -> Self {
        Self::invalid(err.to_string())
    }

    pub(super) fn daemon_unreachable(target: &Reconnector, err: &impl std::fmt::Display) -> Self {
        Self::new(
            ErrorKind::DaemonUnreachable,
            match &target.carrier {
                felis_client_core::Carrier::Local(socket) => {
                    format!("no daemon at {socket}: {err}")
                }
                felis_client_core::Carrier::Ssh { destination, .. } => {
                    format!("cannot reach the felis daemon on {destination}: {err}")
                }
            },
        )
    }

    fn render(&self) -> Value {
        serde_json::json!({ "kind": self.kind.as_str(), "message": self.message })
    }
}

impl From<OutputClosed> for BridgeError {
    fn from(_: OutputClosed) -> Self {
        Self::new(
            ErrorKind::OutputFailed,
            "the bridge's stdout is unavailable",
        )
    }
}

impl From<LinkError> for BridgeError {
    fn from(err: LinkError) -> Self {
        match err {
            LinkError::InvalidRequest(detail) => Self::invalid(detail),
            LinkError::Lost(detail) => Self::new(ErrorKind::DaemonLost, detail),
            LinkError::Protocol(detail) => Self::new(ErrorKind::Protocol, detail),
            LinkError::Refused { reason, detail } => Self::new(
                ErrorKind::from_stream_reason(reason),
                format!("{}: {detail}", stream_error_reason(reason)),
            ),
        }
    }
}

/// Spelled here rather than through `Debug` so the tokens are part of
/// the output contract instead of a by-product of the enum's spelling.
pub(super) const fn stream_error_reason(reason: StreamErrorReason) -> &'static str {
    match reason {
        StreamErrorReason::InvalidRequest => "invalid_request",
        StreamErrorReason::TooManyStreams => "too_many_streams",
        StreamErrorReason::Unavailable => "unavailable",
        StreamErrorReason::Internal => "internal",
    }
}

/// A struct rather than a `serde_json::Map` so the fields keep their
/// declared order: a map sorts its keys, which would put `error` ahead
/// of `v` and make the objects unreadable in a log.
#[derive(serde::Serialize)]
struct Envelope<'a> {
    v: u32,
    id: &'a Value,
    /// Present only on the stream shapes: `end`, `error`, `lag`.
    #[serde(skip_serializing_if = "Option::is_none")]
    event: Option<&'static str>,
    #[serde(flatten)]
    body: Body,
}

/// Externally tagged, so the variant *is* the key a client keys on.
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Body {
    Result(Value),
    Error(Value),
    Item(Value),
    Dropped(u64),
    /// Flattened, because both fields sit beside `event`.
    #[serde(untagged)]
    End {
        count: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<u32>,
    },
}

pub(super) fn envelope(id: &Value, event: Option<&'static str>, body: Body) -> String {
    let object = Envelope {
        v: SURFACE_VERSION,
        id,
        event,
        body,
    };
    // Serialization of plain values cannot fail; a client waiting on a
    // terminal is better served by a placeholder than a dropped line.
    serde_json::to_string(&object).unwrap_or_else(|_| {
        format!(r#"{{"v":{SURFACE_VERSION},"id":null,"error":{{"kind":"internal"}}}}"#)
    })
}

pub(super) fn error_object(id: &Value, err: &BridgeError) -> String {
    envelope(id, None, Body::Error(err.render()))
}

/// The failure shape a request is owed: a stream's `error` terminal,
/// or a point request's `error` reply.
pub(super) fn failure_object(id: &Value, streaming: bool, err: &BridgeError) -> String {
    if streaming {
        error_terminal_object(id, err)
    } else {
        error_object(id, err)
    }
}

/// The point-verb error body under the `event` a stream consumer
/// watches for, so the two are never spelled independently.
pub(super) fn error_terminal_object(id: &Value, err: &BridgeError) -> String {
    envelope(id, Some("error"), Body::Error(err.render()))
}

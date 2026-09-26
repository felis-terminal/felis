//! Session-id prefix resolution against a roster the client holds.
//! The prefix vocabulary is [`felis_protocol::session_prefix`], shared
//! with the daemon's resolver so the two sides agree on what counts as
//! a prefix.

use felis_protocol::SessionHex;
use felis_protocol::messages::SessionInfo;
pub use felis_protocol::session_prefix::{normalize_session_prefix, validate_session_id_prefix};
use thiserror::Error;

/// Each variant's message is printed verbatim by the CLI, remedy
/// included.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SessionPrefixError {
    #[error("no session matches prefix `{prefix}` — try `felis sessions list`")]
    NoMatch { prefix: String },
    #[error("ambiguous prefix `{prefix}` matches {matches} sessions; lengthen the prefix")]
    Ambiguous { prefix: String, matches: usize },
}

pub fn resolve_session_id(
    prefix: &str,
    sessions: &[SessionInfo],
) -> Result<u128, SessionPrefixError> {
    let normalized = normalize_session_prefix(prefix);
    let matches: Vec<u128> = sessions
        .iter()
        .filter(|s| SessionHex(s.id).to_string().starts_with(&normalized))
        .map(|s| s.id)
        .collect();
    match matches.as_slice() {
        [] => Err(SessionPrefixError::NoMatch { prefix: normalized }),
        [id] => Ok(*id),
        many => Err(SessionPrefixError::Ambiguous {
            prefix: normalized,
            matches: many.len(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: u128) -> SessionInfo {
        SessionInfo {
            id,
            dims: felis_protocol::messages::GridDims {
                rows: 24,
                cols: 80,
                pixel_w: 0,
                pixel_h: 0,
            },
            title: None,
            cwd: None,
            idle_seconds: None,
            tags: Vec::new(),
            last_notification: None,
            foreground: None,
            exited: false,
            last_exit_code: None,
            attachments: Vec::new(),
            sequence: std::num::NonZeroU64::MIN,
        }
    }

    #[test]
    fn resolve_session_id_returns_unique_match() {
        // Ids are formatted as 32 zero-padded hex digits, so a small
        // test id like `0x1234` would never match a short prefix.
        let sessions = vec![
            session(0xdead_beef_0000_0000_0000_0000_0000_0001),
            session(0xcafe_babe_0000_0000_0000_0000_0000_0002),
        ];
        assert_eq!(
            resolve_session_id("caf", &sessions).unwrap(),
            0xcafe_babe_0000_0000_0000_0000_0000_0002,
        );
        assert_eq!(
            resolve_session_id("deadbeef000000000000000000000001", &sessions,).unwrap(),
            0xdead_beef_0000_0000_0000_0000_0000_0001,
        );
    }

    #[test]
    fn resolve_session_id_rejects_no_match_with_helpful_hint() {
        let err = resolve_session_id("9999", &[]).unwrap_err().to_string();
        assert!(err.contains("no session matches"));
        assert!(err.contains("sessions list"));
    }

    #[test]
    fn resolve_session_id_rejects_ambiguous_prefix() {
        let sessions = vec![
            session(0xdead_beef_0000_0000_0000_0000_0000_0000),
            session(0xdead_cafe_0000_0000_0000_0000_0000_0000),
        ];
        let err = resolve_session_id("dead", &sessions)
            .unwrap_err()
            .to_string();
        assert!(err.contains("ambiguous"));
        assert!(err.contains("2 sessions"));
    }
}

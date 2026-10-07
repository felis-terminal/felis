//! The state an in-place upgrade hands its successor
//! (`docs/explanation/architecture/overview.md` "In-place upgrade").
//!
//! Private to one exec: read by the immediate successor only, never
//! written to disk, never exposed over IPC.

use serde::{Deserialize, Serialize};

/// The dump version this build writes and the one it reads. A field
/// added with a default needs no bump; only a change a successor could
/// not read from its predecessor does.
pub const DUMP_VERSION: u32 = 1;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Dump {
    pub version: u32,
    /// The listening socket's descriptor number across the exec.
    pub listen_fd: i32,
    /// The next creation sequence, so the successor never reuses one a
    /// window may hold.
    pub next_sequence: u64,
    pub next_attachment_id: u64,
    pub sessions: Vec<SessionDump>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionDump {
    /// The session id as 32 hex digits: JSON numbers cannot carry a
    /// `u128` exactly.
    pub id: String,
    pub sequence: u64,
    pub child: ChildDump,
    pub master_fd: i32,
    /// Input the writer had not handed to the OS, from the exact byte
    /// its last write ended at.
    pub unwritten: Vec<u8>,
    pub rows: u16,
    pub cols: u16,
    pub pixel_w: u16,
    pub pixel_h: u16,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub tags: Vec<String>,
    pub exited: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChildDump {
    pub pid: i32,
    /// The raw wait status of a child the predecessor already reaped.
    /// Present means the pid may belong to another process by now.
    pub exit_status: Option<i32>,
}

#[derive(Debug, thiserror::Error)]
pub enum DumpError {
    #[error("dump version {found} is not the version {DUMP_VERSION} this daemon reads")]
    Version { found: u32 },
    #[error("malformed dump: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("invalid dump: {0}")]
    Invalid(String),
}

impl Dump {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// Decodes and checks everything a restore depends on before any
    /// descriptor is adopted: the version, every id, and that no
    /// descriptor number is named twice.
    pub fn decode(bytes: &[u8]) -> Result<Self, DumpError> {
        #[derive(Deserialize)]
        struct Header {
            #[serde(default)]
            version: u32,
        }
        let header: Header = serde_json::from_slice(bytes)?;
        if !reads_version(header.version) {
            return Err(DumpError::Version {
                found: header.version,
            });
        }
        let dump: Self = serde_json::from_slice(bytes)?;
        dump.validate()?;
        Ok(dump)
    }

    fn validate(&self) -> Result<(), DumpError> {
        let mut fds = std::collections::HashSet::new();
        if self.listen_fd < 0 || !fds.insert(self.listen_fd) {
            return Err(DumpError::Invalid("listen descriptor".to_owned()));
        }
        for session in &self.sessions {
            parse_session_id(&session.id)
                .ok_or_else(|| DumpError::Invalid(format!("session id {:?}", session.id)))?;
            if session.master_fd < 0 || !fds.insert(session.master_fd) {
                return Err(DumpError::Invalid(format!(
                    "master descriptor of session {}",
                    session.id
                )));
            }
            if session.child.pid <= 0 {
                return Err(DumpError::Invalid(format!("pid of session {}", session.id)));
            }
            if session.sequence == 0 || session.sequence >= self.next_sequence {
                return Err(DumpError::Invalid(format!(
                    "sequence of session {}",
                    session.id
                )));
            }
        }
        Ok(())
    }
}

/// Whether this build reads a dump written at `version`.
#[must_use]
pub const fn reads_version(version: u32) -> bool {
    version == DUMP_VERSION
}

#[must_use]
pub fn format_session_id(id: u128) -> String {
    format!("{id:032x}")
}

#[must_use]
pub fn parse_session_id(text: &str) -> Option<u128> {
    (text.len() == 32)
        .then(|| u128::from_str_radix(text, 16).ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Dump {
        Dump {
            version: DUMP_VERSION,
            listen_fd: 3,
            next_sequence: 3,
            next_attachment_id: 7,
            sessions: vec![SessionDump {
                id: format_session_id(0xfeed_u128 << 64 | 1),
                sequence: 2,
                child: ChildDump {
                    pid: 4242,
                    exit_status: None,
                },
                master_fd: 9,
                unwritten: b"queued input".to_vec(),
                rows: 24,
                cols: 80,
                tags: vec!["build".to_owned()],
                ..SessionDump::default()
            }],
        }
    }

    #[test]
    fn a_dump_round_trips() {
        let dump = sample();
        assert_eq!(Dump::decode(&dump.encode()).unwrap(), dump);
    }

    #[test]
    fn a_field_the_writer_did_not_know_takes_its_default() {
        let mut value: serde_json::Value = serde_json::from_slice(&sample().encode()).unwrap();
        value["sessions"][0].as_object_mut().unwrap().remove("tags");
        let decoded = Dump::decode(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(decoded.sessions[0].tags, Vec::<String>::new());
    }

    #[test]
    fn another_version_is_refused_before_the_body_is_read() {
        let body = br#"{"version":2,"sessions":"not even the right shape"}"#;
        assert!(matches!(
            Dump::decode(body),
            Err(DumpError::Version { found: 2 })
        ));
    }

    #[test]
    fn a_descriptor_named_twice_is_refused() {
        let mut dump = sample();
        dump.sessions[0].master_fd = dump.listen_fd;
        assert!(matches!(
            Dump::decode(&dump.encode()),
            Err(DumpError::Invalid(_))
        ));
    }

    #[test]
    fn a_session_id_round_trips_through_its_hex_form() {
        let id = u128::MAX - 5;
        assert_eq!(parse_session_id(&format_session_id(id)), Some(id));
        assert_eq!(parse_session_id("abc"), None);
    }
}

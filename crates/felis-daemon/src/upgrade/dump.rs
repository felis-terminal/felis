//! The state an in-place upgrade hands its successor
//! (`docs/explanation/architecture/overview.md` "In-place upgrade").
//!
//! Private to one exec: read by the immediate successor only, never
//! written to disk, never exposed over IPC.

use felis_grid::{
    Grid, TableGc,
    images::{ImageStore, Placements},
};
use felis_protocol::messages::Urgency;
use felis_vt::{Parser, kitty_graphics::Reassembler};
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
    /// The predecessor's animation clock at the dump, so a frame due
    /// time recorded against it stays due on the successor's.
    pub anim_clock_ms: u64,
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
    /// `None` restores a blank screen.
    pub state: Option<Box<SessionState>>,
}

/// What a session's clients observe that the PTY does not carry: the
/// grid and parser as they stand, the images and their placements, and
/// what the program was last told about focus, color scheme, and size.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionState {
    pub grid: Grid,
    pub parser: Parser,
    pub table_gc: TableGc,
    pub images: ImageStore,
    pub placements: Placements,
    pub saved_primary_placements: Option<Placements>,
    pub reassembler: Reassembler,
    /// Shared memory names whose unlink the session still owes.
    pub shm_segments: Vec<String>,
    pub reported_focus: bool,
    pub reported_os_dark: bool,
    pub reported_resize: Option<(u32, (u16, u16))>,
    /// How long the session had been idle, measured at the dump.
    pub idle_ms: u64,
    pub last_notification: Option<NotificationDump>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            grid: Grid::dump_default(),
            parser: Parser::new(),
            table_gc: TableGc::new(),
            images: ImageStore::new(crate::pool::DEFAULT_IMAGE_BYTE_CAP),
            placements: Placements::new(),
            saved_primary_placements: None,
            reassembler: Reassembler::new(),
            shm_segments: Vec::new(),
            reported_focus: false,
            reported_os_dark: false,
            reported_resize: None,
            idle_ms: 0,
            last_notification: None,
        }
    }
}

/// The grid has no `Debug`, and a whole session is too large to print.
impl std::fmt::Debug for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionState")
            .field("rows", &self.grid.rows())
            .field("cols", &self.grid.cols())
            .field("images", &self.images.len())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationDump {
    pub title: Option<String>,
    pub body: String,
    pub urgency: Urgency,
    pub age_ms: u64,
}

impl SessionState {
    fn check(&self, rows: u16, cols: u16) -> Result<(), String> {
        if (self.grid.rows(), self.grid.cols()) != (rows, cols) {
            return Err("grid size differs from the session's".to_owned());
        }
        self.grid.check_restored().map_err(|err| err.0)?;
        self.parser.check_restored().map_err(|err| err.0)?;
        self.images.check_restored().map_err(|err| err.0)?;
        self.reassembler.check_restored().map_err(|err| err.0)?;
        if self.shm_segments.len() > crate::graphics::ShmDeferral::CAP {
            return Err("deferred shared memory names".to_owned());
        }
        Ok(())
    }
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
            if let Some(state) = &session.state {
                state.check(session.rows, session.cols).map_err(|err| {
                    DumpError::Invalid(format!("state of session {}: {err}", session.id))
                })?;
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
            anim_clock_ms: 0,
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

    fn drawn_state() -> SessionState {
        use felis_grid::images::{CellPos, Extent, ImageEntry, ImageFormat, ImageId, Placement};
        use std::num::NonZeroU16;

        let mut state = SessionState {
            grid: Grid::new(24, 80),
            ..SessionState::default()
        };
        state.parser.advance(
            &mut state.grid,
            b"\x1b]2;title\x07\x1b[31mred\x1b[0m\r\n\x1b[?1049h\x1b[5;5Halt\x1b[38;2;1",
        );
        state
            .images
            .insert(
                ImageId(7),
                ImageEntry::new(2, 1, ImageFormat::Rgba32, vec![1_u8, 2, 3, 4, 5, 6, 7, 8]),
            )
            .unwrap();
        state.placements.upsert(Placement {
            image_id: ImageId(7),
            placement_id: None,
            anchor: CellPos { row: 1, col: 3 },
            cols: Extent::Natural(NonZeroU16::MIN),
            rows: Extent::Natural(NonZeroU16::MIN),
            source: None,
            z_index: -1,
            no_cursor_move: false,
            quiet: 0,
        });
        state.saved_primary_placements = Some(state.placements.clone());
        state.reported_focus = true;
        state.reported_resize = Some((2, (24, 80)));
        state
    }

    fn with_state(state: SessionState) -> Dump {
        let mut dump = sample();
        dump.sessions[0].state = Some(Box::new(state));
        dump
    }

    #[test]
    fn a_session_state_round_trips_with_its_images_and_placements() {
        let dump = with_state(drawn_state());
        let decoded = Dump::decode(&dump.encode()).unwrap();
        assert_eq!(decoded, dump);
        let state = decoded.sessions[0].state.as_ref().unwrap();
        assert_eq!(
            state
                .images
                .get(felis_grid::images::ImageId(7))
                .unwrap()
                .pixels(),
            [1, 2, 3, 4, 5, 6, 7, 8]
        );
        assert_eq!(state.placements.iter().count(), 1);
        assert_eq!(state.grid.title(), Some("title"));
    }

    fn refused(edit: impl FnOnce(&mut serde_json::Value)) -> bool {
        let mut value: serde_json::Value =
            serde_json::from_slice(&with_state(drawn_state()).encode()).unwrap();
        edit(&mut value["sessions"][0]["state"]);
        matches!(
            Dump::decode(&serde_json::to_vec(&value).unwrap()),
            Err(DumpError::Invalid(_) | DumpError::Malformed(_))
        )
    }

    #[test]
    fn inconsistent_screen_state_is_refused_before_anything_is_adopted() {
        assert!(!refused(|_| {}));
        assert!(refused(
            |state| state["grid"]["screen"]["cursor"]["col"] = 80.into()
        ));
        assert!(refused(
            |state| state["grid"]["screen"]["ring"]["cols"] = 81.into()
        ));
        assert!(refused(|state| state["parser"]["params"]["len"] = 17.into()));
        assert!(refused(|state| state["images"]["bytes_total"] = 1.into()));
        assert!(refused(|state| {
            state["images"]["entries"][0][1]["frames"][0]["pixels"] = "AAAA".into();
        }));
        let mut resized = with_state(drawn_state());
        resized.sessions[0].rows = 25;
        assert!(matches!(
            Dump::decode(&resized.encode()),
            Err(DumpError::Invalid(_))
        ));
    }

    /// Measurement (ignored): the dump of one session whose default
    /// scrollback is full of colored 80-column text.
    #[test]
    #[ignore = "measurement; run with --run-ignored all"]
    fn measure_a_full_scrollback_dump() {
        let mut state = SessionState {
            grid: Grid::new(24, 80),
            ..SessionState::default()
        };
        for i in 0..felis_grid::DEFAULT_SCROLLBACK_ROWS + 24 {
            let line = format!(
                "\x1b[3{}m{i:>6} -rw-r--r-- 1 user group  4096 Oct  7 12:00 file-{i}.txt\x1b[0m{}\r\n",
                i % 8,
                " ".repeat(10)
            );
            state.parser.advance(&mut state.grid, line.as_bytes());
        }
        let started = std::time::Instant::now();
        let bytes = with_state(state).encode();
        let encoded = started.elapsed();
        let started = std::time::Instant::now();
        Dump::decode(&bytes).unwrap();
        eprintln!(
            "full-scrollback dump: {} KiB, encode {encoded:?}, decode {:?}",
            bytes.len() / 1024,
            started.elapsed()
        );
    }
}

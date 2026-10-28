//! The JSON bodies of the CLI's session results (docs/reference/cli.md
//! "Machine output"), owned so a consumer can deserialize what the CLI
//! serializes; the JSON protocol reuses them for its session rows.

use felis_protocol::SessionHex;
use serde::{Deserialize, Deserializer, Serialize};

use super::common::session_id_from_hex;

/// Reads a session id only in the spelling the CLI writes it.
fn session_id<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let id = String::deserialize(deserializer)?;
    session_id_from_hex(&id).map_err(serde::de::Error::custom)?;
    Ok(id)
}

/// The body of a `sessions list` item and a `sessions info` result
/// alike.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionObject {
    #[cfg_attr(feature = "schema", schemars(regex(pattern = r"^[0-9a-f]{32}$")))]
    #[serde(deserialize_with = "session_id")]
    pub id: String,
    pub short_id: String,
    pub rows: u16,
    pub cols: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The directory the child was started in; unlike `cwd`, it never
    /// follows the program.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_cwd: Option<String>,
    /// Always present, empty for an untagged session, so a `jq` picker
    /// can rely on `.tags` being iterable.
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_notification: Option<LastNotificationObject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit_code: Option<u32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exited: bool,
    /// Always present, like `tags`: a picker's `.attachments[]` must be
    /// iterable on a parked session too.
    pub attachments: Vec<AttachmentObject>,
    /// OSC 7501 records, sorted by id. Always present, like `tags`.
    pub program_status: Vec<ProgramStatusObject>,
    /// The last `OSC 9 ; 4` progress the program reported and has not
    /// cleared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_progress: Option<TerminalProgressObject>,
}

/// One `OSC 9 ; 4` report. `state` is an open vocabulary, as
/// [`ProgramStatusObject::state`] is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TerminalProgressObject {
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(range(max = 100)))]
    pub value: Option<u8>,
}

/// One OSC 7501 record, every value as the program sent it. `state` and
/// `kind` are open vocabularies: a value this build does not list is a
/// newer one, not an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProgramStatusObject {
    /// `""` is the root record.
    pub id: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(range(max = 100)))]
    pub progress: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg: Option<String>,
}

/// `sessions list`'s whole answer. The roster is bounded and arrives in
/// one frame, so it is a point result: a stream framing would promise a
/// source that can grow or arrive incrementally, and this one cannot
/// (docs/explanation/architecture/control-surfaces.md).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ListResult {
    /// Always present, empty for an empty roster.
    pub sessions: Vec<SessionObject>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LastNotificationObject {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub body: String,
    pub urgency: String,
    pub age_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AttachmentObject {
    /// A decimal **string**: the id is a `u64` counter, and a bare JSON
    /// number past 2^53−1 silently rounds in common consumers.
    /// `--attachment` accepts the same spelling.
    #[cfg_attr(feature = "schema", schemars(regex(pattern = r"^[0-9]+$")))]
    pub id: String,
    /// RFC 3339, UTC.
    pub attached_at: String,
    /// Unconditional, unlike the optionals elsewhere: an omitted key
    /// would read as "this daemon does not report it".
    pub input_owner: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionRef {
    #[cfg_attr(feature = "schema", schemars(regex(pattern = r"^[0-9a-f]{32}$")))]
    #[serde(deserialize_with = "session_id")]
    pub id: String,
    /// `evict`'s flag; absent on other verbs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub was_attached: Option<bool>,
    /// `send --wait`'s observed mark code. Absent for a bare
    /// `OSC 133 ; D`, and on the verbs that never wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<u32>,
}

impl SessionRef {
    pub fn new(id: u128) -> Self {
        Self {
            id: SessionHex(id).to_string(),
            was_attached: None,
            exit_code: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TagResult {
    #[cfg_attr(feature = "schema", schemars(regex(pattern = r"^[0-9a-f]{32}$")))]
    #[serde(deserialize_with = "session_id")]
    pub id: String,
    /// Sorted by the daemon and always present, matching `list` / `info`.
    pub tags: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running() -> SessionObject {
        SessionObject {
            id: SessionHex(0x1a2b).to_string(),
            short_id: "0000".to_owned(),
            rows: 24,
            cols: 80,
            idle_seconds: Some(3),
            title: Some("vim".to_owned()),
            cwd: Some("/home/user/src".to_owned()),
            spawn_cwd: Some("/home/user".to_owned()),
            tags: vec!["build".to_owned()],
            last_notification: Some(LastNotificationObject {
                title: None,
                body: "done".to_owned(),
                urgency: "normal".to_owned(),
                age_seconds: 7,
            }),
            foreground: Some("vim".to_owned()),
            last_exit_code: Some(0),
            exited: false,
            attachments: vec![AttachmentObject {
                id: "18446744073709551615".to_owned(),
                attached_at: "2026-01-02T03:04:05Z".to_owned(),
                input_owner: true,
            }],
            program_status: vec![ProgramStatusObject {
                id: String::new(),
                state: "busy".to_owned(),
                kind: Some("build".to_owned()),
                progress: Some(40),
                app: None,
                title: None,
                msg: Some("linking".to_owned()),
            }],
            terminal_progress: Some(TerminalProgressObject {
                state: "normal".to_owned(),
                value: Some(40),
            }),
        }
    }

    fn exited() -> SessionObject {
        SessionObject {
            id: SessionHex(0xCAFE).to_string(),
            short_id: "0000".to_owned(),
            rows: 24,
            cols: 80,
            idle_seconds: None,
            title: None,
            cwd: None,
            spawn_cwd: None,
            tags: Vec::new(),
            last_notification: None,
            foreground: None,
            last_exit_code: Some(2),
            exited: true,
            attachments: Vec::new(),
            program_status: Vec::new(),
            terminal_progress: None,
        }
    }

    fn round_trip(session: &SessionObject) -> SessionObject {
        let text = serde_json::to_string(session).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn a_running_session_object_round_trips_through_json() {
        let session = running();
        assert_eq!(round_trip(&session), session);
    }

    #[test]
    fn an_exited_session_object_round_trips_through_json() {
        let session = exited();
        assert_eq!(round_trip(&session), session);
    }

    #[test]
    fn a_running_session_without_its_omitted_keys_reads_back_as_not_exited() {
        let mut session = running();
        session.idle_seconds = None;
        session.title = None;
        session.terminal_progress = None;
        let value = serde_json::to_value(&session).unwrap();
        for key in ["exited", "idle_seconds", "title", "terminal_progress"] {
            assert!(value.get(key).is_none(), "{key} was serialized: {value}");
        }
        let read: SessionObject = serde_json::from_value(value).unwrap();
        assert!(!read.exited);
        assert_eq!(read, session);
    }

    #[test]
    fn a_session_id_in_another_spelling_is_refused() {
        for id in ["xyz", "0123456789ABCDEF0123456789ABCDEF", "1a2b"] {
            let mut session = serde_json::to_value(running()).unwrap();
            session["id"] = serde_json::json!(id);
            assert!(
                serde_json::from_value::<SessionObject>(session).is_err(),
                "{id}"
            );
            let reference = serde_json::json!({ "id": id });
            assert!(
                serde_json::from_value::<SessionRef>(reference).is_err(),
                "{id}"
            );
            let tags = serde_json::json!({ "id": id, "tags": [] });
            assert!(serde_json::from_value::<TagResult>(tags).is_err(), "{id}");
        }
    }

    #[test]
    fn a_session_ref_without_the_optional_flags_reads_back_with_them_absent() {
        let read: SessionRef =
            serde_json::from_str(&format!(r#"{{"id":"{}"}}"#, SessionHex(0x1a2b))).unwrap();
        assert_eq!(read, SessionRef::new(0x1a2b));
    }
}

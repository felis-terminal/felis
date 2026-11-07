//! The JSON bodies of the CLI's session results (docs/reference/cli.md
//! "Machine output"), owned so a consumer can deserialize what the CLI
//! serializes; the JSON protocol reuses them for its session rows.

use std::num::NonZeroU32;

use felis_protocol::SessionHex;
use felis_protocol::messages::{AttachCarrier, SessionInfo, SshEnv};
use felis_protocol::session_prefix::short_session_prefix;
use serde::{Deserialize, Deserializer, Serialize};

use super::common::session_id_from_hex;
use super::timestamp::rfc3339_utc;

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

impl SessionObject {
    /// `short_id` is a display form, never a persistence target: its
    /// uniqueness holds for the roster it was computed against, so a
    /// stored prefix can match a younger session later
    /// (docs/reference/cli.md "Machine output").
    #[must_use]
    pub fn from_info(s: &SessionInfo, short_id: String) -> Self {
        Self {
            id: SessionHex(s.id).to_string(),
            short_id,
            rows: s.dims.rows,
            cols: s.dims.cols,
            idle_seconds: s.idle_seconds,
            title: s.title.clone(),
            cwd: s.cwd.clone(),
            spawn_cwd: s.spawn_cwd.clone(),
            tags: s.tags.clone(),
            last_notification: s
                .last_notification
                .as_ref()
                .map(|n| LastNotificationObject {
                    title: n.notification.title.clone(),
                    body: n.notification.body.clone(),
                    urgency: n.notification.urgency.as_str().to_owned(),
                    age_seconds: n.age_seconds,
                }),
            foreground: s.foreground.clone(),
            last_exit_code: s.last_exit_code,
            exited: s.exited,
            attachments: s
                .attachments
                .iter()
                .map(|a| {
                    let (carrier, ssh_env, client_pid) = match &a.carrier {
                        None => (None, None, None),
                        Some(AttachCarrier::Local { client_pid }) => {
                            (Some(CarrierName::Local), None, *client_pid)
                        }
                        Some(AttachCarrier::Relay { ssh_env }) => (
                            Some(CarrierName::Relay),
                            ssh_env.as_ref().map(SshEnvObject::from),
                            None,
                        ),
                    };
                    AttachmentObject {
                        id: a.id.to_string(),
                        attached_at: rfc3339_utc(a.attached_at),
                        input_owner: a.input_owner,
                        carrier,
                        ssh_env,
                        client_pid,
                    }
                })
                .collect(),
            program_status: s.program_status.iter().map(Into::into).collect(),
            terminal_progress: s
                .terminal_progress
                .as_ref()
                .map(|p| TerminalProgressObject {
                    state: p.state.clone(),
                    value: p.value,
                }),
        }
    }

    /// Shortened against `roster`, which a listing and a single-session
    /// view of it share so both show one session the same `short_id`.
    #[must_use]
    pub fn in_roster(s: &SessionInfo, roster: &[SessionInfo]) -> Self {
        Self::from_info(s, roster_short_id(s.id, roster))
    }
}

/// The git-style unique prefix of `id` among `roster`.
#[must_use]
pub fn roster_short_id(id: u128, roster: &[SessionInfo]) -> String {
    short_session_prefix(id, roster.iter().map(|s| s.id))
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

impl From<&felis_protocol::messages::ProgramStatusRecord> for ProgramStatusObject {
    fn from(r: &felis_protocol::messages::ProgramStatusRecord) -> Self {
        Self {
            id: r.id.clone(),
            state: r.state.clone(),
            kind: r.kind.clone(),
            progress: r.progress,
            app: r.app.clone(),
            title: r.title.clone(),
            msg: r.msg.clone(),
        }
    }
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
    /// How this window's connection reached the daemon, not where its
    /// viewer sits. Omitted when the daemon does not report it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carrier: Option<CarrierName>,
    /// Present only for a `relay` attachment whose SSH variables the
    /// daemon knows; an empty object means it set none. Omitted beside
    /// `relay`, the values are unknown, not unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_env: Option<SshEnvObject>,
    /// The pid of the process that connected this window's socket, for
    /// mapping the attachment to its OS window. Only beside `local`, and
    /// omitted when the daemon does not know it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_pid: Option<NonZeroU32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CarrierName {
    Local,
    Relay,
}

/// Keyed by the variable names, so a hook can export each key as it
/// stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SshEnvObject {
    #[serde(
        rename = "SSH_CONNECTION",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub connection: Option<String>,
    #[serde(
        rename = "SSH_CLIENT",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub client: Option<String>,
    #[serde(rename = "SSH_TTY", default, skip_serializing_if = "Option::is_none")]
    pub tty: Option<String>,
}

impl From<&SshEnv> for SshEnvObject {
    fn from(e: &SshEnv) -> Self {
        Self {
            connection: e.connection.clone(),
            client: e.client.clone(),
            tty: e.tty.clone(),
        }
    }
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
    use crate::test_support::session_info;

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
            attachments: vec![
                AttachmentObject {
                    id: "18446744073709551615".to_owned(),
                    attached_at: "2026-01-02T03:04:05Z".to_owned(),
                    input_owner: true,
                    carrier: Some(CarrierName::Relay),
                    ssh_env: Some(SshEnvObject {
                        connection: Some("192.0.2.7 50122 192.0.2.1 22".to_owned()),
                        client: None,
                        tty: None,
                    }),
                    client_pid: None,
                },
                AttachmentObject {
                    id: "3".to_owned(),
                    attached_at: "2026-01-02T03:04:06Z".to_owned(),
                    input_owner: false,
                    carrier: Some(CarrierName::Local),
                    ssh_env: None,
                    client_pid: NonZeroU32::new(4242),
                },
            ],
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

    /// The wire carries an instant; the JSON row publishes an RFC 3339
    /// string, and an attached session carries no `idle_seconds` key.
    #[test]
    fn an_attachment_renders_its_stamp_as_rfc_3339() {
        let session = SessionInfo {
            attachments: vec![felis_protocol::messages::Attachment {
                id: 7,
                attached_at: std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_788_250_500),
                input_owner: true,
                carrier: None,
            }],
            ..session_info(0xCAFE)
        };
        let body = serde_json::to_value(SessionObject::in_roster(
            &session,
            std::slice::from_ref(&session),
        ))
        .unwrap();
        assert_eq!(
            body["attachments"][0]["attached_at"],
            "2026-09-01T08:15:00Z"
        );
        assert_eq!(body.get("idle_seconds"), None);
    }

    /// The JSON keys are the variable names; an unknown relay environment
    /// omits `ssh_env` where a known-empty one writes `{}`, and an unknown
    /// pid omits `client_pid`.
    #[test]
    fn an_attachment_reports_its_carrier_ssh_variables_and_client_pid() {
        use felis_protocol::messages::Attachment;

        let attachment = |id, carrier| Attachment {
            id,
            attached_at: std::time::UNIX_EPOCH,
            input_owner: false,
            carrier,
        };
        let session = SessionInfo {
            attachments: vec![
                attachment(
                    1,
                    Some(AttachCarrier::Local {
                        client_pid: NonZeroU32::new(4242),
                    }),
                ),
                attachment(
                    2,
                    Some(AttachCarrier::Relay {
                        ssh_env: Some(SshEnv {
                            connection: Some("192.0.2.7 50122 192.0.2.1 22".into()),
                            ..SshEnv::default()
                        }),
                    }),
                ),
                attachment(
                    3,
                    Some(AttachCarrier::Relay {
                        ssh_env: Some(SshEnv::default()),
                    }),
                ),
                attachment(4, Some(AttachCarrier::Relay { ssh_env: None })),
                attachment(5, None),
                attachment(6, Some(AttachCarrier::Local { client_pid: None })),
            ],
            ..session_info(0xCAFE)
        };
        let body = serde_json::to_value(SessionObject::in_roster(
            &session,
            std::slice::from_ref(&session),
        ))
        .unwrap();
        let attached_at = "1970-01-01T00:00:00Z";
        assert_eq!(
            body["attachments"],
            serde_json::json!([
                {
                    "id": "1", "attached_at": attached_at, "input_owner": false, "carrier": "local",
                    "client_pid": 4242,
                },
                {
                    "id": "2", "attached_at": attached_at, "input_owner": false, "carrier": "relay",
                    "ssh_env": { "SSH_CONNECTION": "192.0.2.7 50122 192.0.2.1 22" },
                },
                { "id": "3", "attached_at": attached_at, "input_owner": false, "carrier": "relay", "ssh_env": {} },
                { "id": "4", "attached_at": attached_at, "input_owner": false, "carrier": "relay" },
                { "id": "5", "attached_at": attached_at, "input_owner": false },
                { "id": "6", "attached_at": attached_at, "input_owner": false, "carrier": "local" },
            ])
        );
    }

    #[test]
    fn a_session_ref_without_the_optional_flags_reads_back_with_them_absent() {
        let read: SessionRef =
            serde_json::from_str(&format!(r#"{{"id":"{}"}}"#, SessionHex(0x1a2b))).unwrap();
        assert_eq!(read, SessionRef::new(0x1a2b));
    }
}

//! `felis-json` v1: the named JSON form of a daemon frame body
//! (`docs/reference/ipc.md` "Structural session JSON"). The DTOs here
//! are the contract, converted from and to the domain types
//! explicitly, so a domain rename fails to compile instead of
//! reshaping a `.fcast` file or a deployed browser view.

use felis_protocol::codec::EitherHalf;
use felis_protocol::messages::{
    ConnToClientMsg, ConnToDaemonMsg, GridMsg, ImageMsg, InputMsg, SessionToClientMsg,
    SessionToDaemonMsg,
};
use felis_protocol::{MessageKind, codec};
use serde_json::{Value, json};

use crate::wire::RowCodecError;

/// Every DTO carries the same derives, and every one of them is part
/// of the published schema.
macro_rules! json_dto {
    ($(
        $(#[$meta:meta])*
        $vis:vis $kind:ident $name:ident $body:tt
    )*) => {
        $(
            #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
            #[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
            $(#[$meta])*
            $vis $kind $name $body
        )*
    };
}

mod common;
mod conn;
mod grid;
mod image;
mod input;
mod row;
#[cfg(feature = "schema")]
pub mod schema;
mod session;

pub use common::{
    GridDimsJson, RequestedDimsJson, RgbJson, SESSION_ID_PATTERN, session_id_from_hex,
    session_id_hex,
};
pub use conn::{
    BuildIdentityJson, ConnJson, ConnectionModeJson, RefusalReasonJson, StreamErrorReasonJson,
    SubjectJson,
};
pub use grid::{
    AttentionSourceJson, CLIPBOARD_SELECTION_MAX, ClipboardWriteJson, CursorStyleJson, GridJson,
    KITTY_KBD_FLAGS_MAX, ModifyOtherKeysJson, MouseProtocolJson, PaletteActionJson, PromptKindJson,
    RowJson, ScrollDirectionJson, ThemeActionJson, ThemeChannelJson,
};
pub use image::{ImageFormatJson, ImageJson, ImageTargetJson, SourceRectJson};
pub use row::{
    ASCII_MAX, ASCII_MIN, ATTR_FLAGS_MAX, AttrRunJson, AttributesJson, ColorJson, GraphemeJson,
    HAlignJson, RowCellsJson, SizedCellJson, SizingJson, UnderlineStyleJson, VAlignJson,
};

pub use input::{
    F_KEY_MAX, F_KEY_MIN, INPUT_MODS_MAX, InputJson, KEY_MODS_MAX, KeyEventJson, KeyEventKindJson,
    KeyJson, KeyLocationJson, MouseActionJson, MouseButtonJson, MouseEventJson, NamedKeyJson,
    PromptJumpJson,
};
pub use session::{
    AttachFailureJson, AttachTargetJson, AttachmentJson, EnvBytesJson, EnvPairJson,
    NotificationJson, SessionInfoJson, SessionJson, SessionNotificationJson, SpawnArgsJson,
    TimestampJson, UrgencyJson,
};

/// The value of the envelope's `felis_json` key.
pub const FORMAT_VERSION: u64 = 1;

/// The envelope key that names the format version.
pub const VERSION_KEY: &str = "felis_json";

#[derive(Debug, thiserror::Error)]
pub enum JsonError {
    #[error("frame codec: {0}")]
    Codec(#[from] codec::CodecError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("row codec: {0}")]
    Row(#[from] RowCodecError),
    #[error("not a felis-json envelope: {0}")]
    Envelope(&'static str),
    #[error("felis_json {found} is not {FORMAT_VERSION}")]
    Version { found: Value },
    #[error("`{0}` is not a felis-json v1 frame kind")]
    Kind(String),
    #[error("field `{field}`: {detail}")]
    Field {
        field: &'static str,
        detail: &'static str,
    },
    /// An arm the format deliberately leaves out, rather than one it
    /// failed to convert.
    #[error("`{0}` is outside felis-json v1")]
    Outside(&'static str),
}

impl JsonError {
    pub(crate) const fn field(field: &'static str, detail: &'static str) -> Self {
        Self::Field { field, detail }
    }
}

/// One frame body in its v1 form, tagged by the family it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Grid(GridJson),
    Image(ImageJson),
    Conn(ConnJson),
    Session(SessionJson),
    Input(InputJson),
}

/// The kind tokens the envelope's `kind` key carries.
const GRID: &str = "grid";
const IMAGE: &str = "image";
const CONN: &str = "conn";
const SESSION: &str = "session";
const INPUT: &str = "input";

/// Every frame kind v1 covers, in envelope spelling. The families this
/// list omits are the CLI bridge's own contract, not this format's.
pub const KINDS: [&str; 5] = [GRID, IMAGE, CONN, SESSION, INPUT];

impl Frame {
    /// The envelope token for this frame's family.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Grid(_) => GRID,
            Self::Image(_) => IMAGE,
            Self::Conn(_) => CONN,
            Self::Session(_) => SESSION,
            Self::Input(_) => INPUT,
        }
    }

    /// The wire frame kind this frame decodes from and encodes to.
    #[must_use]
    pub const fn message_kind(&self) -> MessageKind {
        match self {
            Self::Grid(_) => MessageKind::Grid,
            Self::Image(_) => MessageKind::Image,
            Self::Conn(_) => MessageKind::Conn,
            Self::Session(_) => MessageKind::Session,
            Self::Input(_) => MessageKind::Input,
        }
    }

    /// Reads a protobuf frame body into its v1 form.
    ///
    /// # Errors
    ///
    /// When `kind` is outside [`KINDS`] or the body does not decode.
    pub fn from_body(kind: MessageKind, body: &[u8]) -> Result<Self, JsonError> {
        Ok(match kind {
            MessageKind::Grid => Self::Grid(codec::decode::<GridMsg>(body)?.try_into()?),
            MessageKind::Image => Self::Image(codec::decode::<ImageMsg>(body)?.into()),
            MessageKind::Conn => {
                Self::Conn(codec::decode_either::<ConnToDaemonMsg, ConnToClientMsg>(body)?.into())
            }
            MessageKind::Session => Self::Session(
                codec::decode_either::<SessionToDaemonMsg, SessionToClientMsg>(body)?.try_into()?,
            ),
            MessageKind::Input => Self::Input(codec::decode::<InputMsg>(body)?.into()),
            other => return Err(JsonError::Kind(format!("{other:?}"))),
        })
    }

    /// Inverse of [`Self::from_body`].
    ///
    /// # Errors
    ///
    /// When a value the domain type does not admit reaches it.
    pub fn into_body(self) -> Result<(MessageKind, Vec<u8>), JsonError> {
        Ok(match self {
            Self::Grid(msg) => (MessageKind::Grid, codec::encode(&GridMsg::try_from(msg)?)),
            Self::Image(msg) => (MessageKind::Image, codec::encode(&ImageMsg::try_from(msg)?)),
            Self::Conn(msg) => (
                MessageKind::Conn,
                codec::encode_either(&EitherHalf::<ConnToDaemonMsg, ConnToClientMsg>::try_from(
                    msg,
                )?),
            ),
            Self::Session(msg) => (
                MessageKind::Session,
                codec::encode_either(
                    &EitherHalf::<SessionToDaemonMsg, SessionToClientMsg>::try_from(msg)?,
                ),
            ),
            Self::Input(msg) => (MessageKind::Input, codec::encode(&InputMsg::try_from(msg)?)),
        })
    }

    /// The `msg` payload alone, without the envelope.
    ///
    /// # Errors
    ///
    /// When the DTO does not serialize.
    pub fn to_msg(&self) -> Result<Value, JsonError> {
        Ok(match self {
            Self::Grid(msg) => serde_json::to_value(msg)?,
            Self::Image(msg) => serde_json::to_value(msg)?,
            Self::Conn(msg) => serde_json::to_value(msg)?,
            Self::Session(msg) => serde_json::to_value(msg)?,
            Self::Input(msg) => serde_json::to_value(msg)?,
        })
    }

    /// Inverse of [`Self::to_msg`], for a `kind` from [`KINDS`].
    ///
    /// # Errors
    ///
    /// When `kind` is not a v1 kind, or `msg` does not match its DTO.
    pub fn from_msg(kind: &str, msg: &Value) -> Result<Self, JsonError> {
        Ok(match kind {
            GRID => Self::Grid(serde_json::from_value(msg.clone())?),
            IMAGE => Self::Image(serde_json::from_value(msg.clone())?),
            CONN => Self::Conn(serde_json::from_value(msg.clone())?),
            SESSION => Self::Session(serde_json::from_value(msg.clone())?),
            INPUT => Self::Input(serde_json::from_value(msg.clone())?),
            other => return Err(JsonError::Kind(other.to_owned())),
        })
    }
}

/// Wraps a protobuf frame body in the v1 envelope.
///
/// # Errors
///
/// As [`Frame::from_body`].
pub fn encode(kind: MessageKind, body: &[u8]) -> Result<Value, JsonError> {
    let frame = Frame::from_body(kind, body)?;
    Ok(json!({
        VERSION_KEY: FORMAT_VERSION,
        "kind": frame.kind(),
        "msg": frame.to_msg()?,
    }))
}

/// Inverse of [`encode`]; the version is read before the payload is.
///
/// # Errors
///
/// When the envelope is malformed or names another version.
pub fn decode(value: &Value) -> Result<(MessageKind, Vec<u8>), JsonError> {
    let object = value
        .as_object()
        .ok_or(JsonError::Envelope("the envelope is not an object"))?;
    let version = object
        .get(VERSION_KEY)
        .ok_or(JsonError::Envelope("no `felis_json` key"))?;
    if version.as_u64() != Some(FORMAT_VERSION) {
        return Err(JsonError::Version {
            found: version.clone(),
        });
    }
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .ok_or(JsonError::Envelope("`kind` is not a string"))?;
    let msg = object
        .get("msg")
        .ok_or(JsonError::Envelope("no `msg` key"))?;
    Frame::from_msg(kind, msg)?.into_body()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn welcome() -> Value {
        let msg = ConnToClientMsg::Welcome { identity: None };
        encode(MessageKind::Conn, &codec::encode(&msg)).expect("a Conn frame encodes")
    }

    // A Conn frame carries no row payload: the kind dispatch alone
    // must round-trip it.
    #[test]
    fn a_conn_frame_survives_wire_json_wire() {
        let msg = ConnToClientMsg::Welcome { identity: None };
        let value = encode(MessageKind::Conn, &codec::encode(&msg)).unwrap();
        assert_eq!(value["felis_json"], json!(1));
        assert_eq!(value["kind"], json!("conn"));
        let (kind, body) = decode(&value).unwrap();
        assert_eq!(kind, MessageKind::Conn);
        assert_eq!(codec::decode::<ConnToClientMsg>(&body).unwrap(), msg);
    }

    #[test]
    fn a_future_format_version_is_refused_before_the_payload() {
        let mut value = welcome();
        value["felis_json"] = json!(2);
        value["msg"] = json!("not a message at all");
        assert!(matches!(decode(&value), Err(JsonError::Version { .. })));
    }

    #[test]
    fn an_unknown_kind_is_refused() {
        let mut value = welcome();
        value["kind"] = json!("ops");
        assert!(matches!(decode(&value), Err(JsonError::Kind(_))));
    }

    /// A reader must ignore what a later v1 writer adds, which is the
    /// whole compatibility promise inside the epoch.
    #[test]
    fn an_unknown_optional_field_still_decodes() {
        let mut value = welcome();
        value["msg"]["invented_later"] = json!({ "any": "shape" });
        value["also_invented_later"] = json!(7);
        assert!(decode(&value).is_ok());
    }
}

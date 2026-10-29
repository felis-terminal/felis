//! The JSON surfaces client-core owns: the CLI's session result bodies
//! and the JSON protocol a relay speaks to a browser view
//! (`docs/reference/json-protocol.md`), whose payloads are dedicated
//! DTOs so a domain rename fails to compile instead of reshaping them.

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

macro_rules! plain_enum {
    ($json:ty, $domain:ty, $($variant:ident),+ $(,)?) => {
        impl From<$domain> for $json {
            fn from(value: $domain) -> Self {
                match value {
                    $(<$domain>::$variant => Self::$variant,)+
                }
            }
        }

        impl From<$json> for $domain {
            fn from(value: $json) -> Self {
                match value {
                    $(<$json>::$variant => Self::$variant,)+
                }
            }
        }
    };
}

mod common;
mod grid;
mod image;
mod input;
mod protocol;
#[cfg(feature = "schema")]
pub mod schema;
mod session;
mod spawn;

pub use common::{
    BASE64_PATTERN, Base64, DtoError, GridDimsJson, RequestedDimsJson, RgbJson, SESSION_ID_PATTERN,
    SessionId, ViewportAnchorJson, session_id_from_hex, session_id_hex,
};
pub use felis_grid::row_json;
pub use grid::{
    AttentionSourceJson, CLIPBOARD_SELECTION_MAX, ClipboardWriteJson, CursorStyleJson, GridJson,
    KITTY_KBD_FLAGS_MAX, ModifyOtherKeysJson, MouseProtocolJson, PaletteActionJson, PromptKindJson,
    RowJson, ScrollDirectionJson, ThemeActionJson, ThemeChannelJson,
};
pub use image::{ImageFormatJson, ImageJson, ImageTargetJson, RequestedExtentJson, SourceRectJson};
pub use input::{
    F_KEY_MAX, F_KEY_MIN, INPUT_MODS_MAX, InputJson, KEY_MODS_MAX, KeyEventJson, KeyEventKindJson,
    KeyJson, KeyLocationJson, MouseActionJson, MouseButtonJson, MouseEventJson, NamedKeyJson,
    PromptJumpJson,
};
pub use protocol::{
    AttachTarget, ByteSpanJson, CaptureRowJson, ClientHello, ClientMessage, ColSpanJson,
    CorrelationId, DaemonRequest, ErrorKind, ErrorObject, FramingError, Item, Ledger,
    NotificationJson, OpResult, Open, RegionSourceJson, Request, SUPPORTED_VERSIONS,
    SearchMatchJson, SearchSourceJson, SendStepJson, ServerHello, ServerMessage, SessionRow,
    StreamKind, UrgencyJson, negotiate,
};
pub use session::{
    AttachmentObject, LastNotificationObject, ListResult, ProgramStatusObject, SessionObject,
    SessionRef, TagResult, TerminalProgressObject,
};
pub use spawn::{EnvBytesJson, EnvPairJson, SpawnArgsJson};

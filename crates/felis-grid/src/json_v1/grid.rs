//! v1 DTO for [`GridMsg`], the daemon→client grid diffs.

use super::JsonError;
use super::common::{GridDimsJson, RgbJson};
use super::row::{self, RowCellsJson};
use crate::StyleTable;
use felis_protocol::RowPayload;
use felis_protocol::kitty_keyboard::KittyKbdFlags;
use felis_protocol::messages::{
    AttentionSource, ClipboardSelection, ClipboardWrite, CursorStyle, GridMsg, ModifyOtherKeys,
    MouseProtocol, PaletteAction, PromptKind, ScrollDirection, ThemeAction, ThemeChannel,
};

/// Every clipboard selector this build defines.
pub const CLIPBOARD_SELECTION_MAX: u8 = ClipboardSelection::all().bits();

/// Every Kitty keyboard progressive-enhancement bit this build defines.
pub const KITTY_KBD_FLAGS_MAX: u8 = KittyKbdFlags::all().bits();

json_dto! {
    /// One dirty row of a [`GridJson::RowDelta`]. The cells ride the
    /// row codec's structural form (`docs/reference/row-codec.md`),
    /// never the packed bytes the socket carries.
    pub struct RowJson {
        /// Zero-based.
        pub row: u16,
        pub cells: RowCellsJson,
    }

    /// `DECSCUSR` shape.
    #[serde(rename_all = "snake_case")]
    pub enum CursorStyleJson {
        Block,
        Underline,
        Bar,
    }

    #[serde(rename_all = "snake_case")]
    pub enum ScrollDirectionJson {
        Up,
        Down,
    }

    /// `OSC 133` boundary kind.
    #[serde(rename_all = "snake_case")]
    pub enum PromptKindJson {
        PromptStart,
        InputStart,
        OutputStart,
        CommandEnd,
    }

    #[serde(rename_all = "snake_case")]
    pub enum ThemeChannelJson {
        Foreground,
        Background,
        Cursor,
    }

    #[serde(rename_all = "snake_case")]
    pub enum MouseProtocolJson {
        Off,
        ButtonEvents,
        ButtonAndDrag,
        AnyMotion,
    }

    /// xterm `modifyOtherKeys` level.
    #[serde(rename_all = "snake_case")]
    pub enum ModifyOtherKeysJson {
        Off,
        Level1,
        Level2,
    }

    #[serde(rename_all = "snake_case")]
    pub enum AttentionSourceJson {
        Bell,
        Notification,
    }

    /// What a [`GridJson::PaletteColor`] does to its slot.
    #[serde(tag = "action", rename_all = "snake_case")]
    pub enum PaletteActionJson {
        Set { rgb: RgbJson },
        Reset,
    }

    /// What a [`GridJson::ThemeColor`] does to its channel.
    #[serde(tag = "action", rename_all = "snake_case")]
    pub enum ThemeActionJson {
        Set { rgb: RgbJson },
        Reset,
    }

    /// One `OSC 52` write request.
    pub struct ClipboardWriteJson {
        /// Bit 0 is the clipboard, bit 1 X11 PRIMARY
        /// (`docs/reference/ipc.md`).
        #[cfg_attr(feature = "schema", schemars(range(max = CLIPBOARD_SELECTION_MAX)))]
        pub selection: u8,
        /// Decoded bytes, post-base64.
        pub data: Vec<u8>,
    }

    /// Grid-family frames in their v1 form.
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum GridJson {
        RehydrateBegin,
        RehydrateEnd,
        RowDelta {
            rows: Vec<RowJson>,
        },
        Scrolled {
            /// Inclusive.
            region_top: u16,
            /// Inclusive.
            region_bottom: u16,
            n_rows: u16,
            direction: ScrollDirectionJson,
        },
        Cluster {
            /// 1-based; `0` is reserved.
            id: u32,
            text: String,
        },
        Hyperlink {
            /// 1-based; `0` is reserved.
            id: u16,
            anchor: Option<String>,
            uri: String,
        },
        CursorState {
            row: u16,
            col: u16,
            visible: bool,
            style: CursorStyleJson,
            blink: bool,
        },
        ViewportState {
            lines_from_bottom: u32,
            max: u32,
        },
        Size {
            dims: GridDimsJson,
        },
        Title {
            value: String,
        },
        Cwd {
            value: String,
        },
        PromptMark {
            line: u64,
            kind: PromptKindJson,
            exit_code: Option<u32>,
        },
        ThemeColor {
            channel: ThemeChannelJson,
            action: ThemeActionJson,
        },
        PaletteColor {
            index: u8,
            action: PaletteActionJson,
        },
        PaletteResetAll,
        PointerShape {
            /// CSS cursor keyword, `null` to reset.
            name: Option<String>,
        },
        KittyKbdFlags {
            /// The Kitty progressive-enhancement bitmap
            /// (`docs/reference/protocols/kitty-keyboard.md`).
            #[cfg_attr(feature = "schema", schemars(range(max = KITTY_KBD_FLAGS_MAX)))]
            flags: u8,
        },
        ModeFlags {
            bracketed_paste: bool,
            alt_screen: bool,
            mouse_protocol: MouseProtocolJson,
            application_cursor: bool,
            modify_other_keys: ModifyOtherKeysJson,
            application_keypad: bool,
            win32_input_mode: bool,
            reverse_video: bool,
        },
        Attention {
            source: AttentionSourceJson,
        },
        ClipboardSet {
            write: ClipboardWriteJson,
        },
        CycleEnd,
    }
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

plain_enum!(CursorStyleJson, CursorStyle, Block, Underline, Bar);
plain_enum!(ScrollDirectionJson, ScrollDirection, Up, Down);
plain_enum!(
    PromptKindJson,
    PromptKind,
    PromptStart,
    InputStart,
    OutputStart,
    CommandEnd
);
plain_enum!(
    ThemeChannelJson,
    ThemeChannel,
    Foreground,
    Background,
    Cursor
);
plain_enum!(
    MouseProtocolJson,
    MouseProtocol,
    Off,
    ButtonEvents,
    ButtonAndDrag,
    AnyMotion
);
plain_enum!(ModifyOtherKeysJson, ModifyOtherKeys, Off, Level1, Level2);
plain_enum!(AttentionSourceJson, AttentionSource, Bell, Notification);

pub(super) use plain_enum;

impl From<PaletteAction> for PaletteActionJson {
    fn from(action: PaletteAction) -> Self {
        match action {
            PaletteAction::Set { rgb } => Self::Set { rgb: rgb.into() },
            PaletteAction::Reset => Self::Reset,
        }
    }
}

impl From<PaletteActionJson> for PaletteAction {
    fn from(action: PaletteActionJson) -> Self {
        match action {
            PaletteActionJson::Set { rgb } => Self::Set { rgb: rgb.into() },
            PaletteActionJson::Reset => Self::Reset,
        }
    }
}

impl From<ThemeAction> for ThemeActionJson {
    fn from(action: ThemeAction) -> Self {
        match action {
            ThemeAction::Set { rgb } => Self::Set { rgb: rgb.into() },
            ThemeAction::Reset => Self::Reset,
        }
    }
}

impl From<ThemeActionJson> for ThemeAction {
    fn from(action: ThemeActionJson) -> Self {
        match action {
            ThemeActionJson::Set { rgb } => Self::Set { rgb: rgb.into() },
            ThemeActionJson::Reset => Self::Reset,
        }
    }
}

impl From<ClipboardWrite> for ClipboardWriteJson {
    fn from(write: ClipboardWrite) -> Self {
        Self {
            selection: write.selection.bits(),
            data: write.data,
        }
    }
}

impl TryFrom<ClipboardWriteJson> for ClipboardWrite {
    type Error = JsonError;

    fn try_from(write: ClipboardWriteJson) -> Result<Self, JsonError> {
        Ok(Self {
            selection: ClipboardSelection::from_bits(write.selection).ok_or_else(|| {
                JsonError::field("selection", "a reserved clipboard selector bit is set")
            })?,
            data: write.data,
        })
    }
}

impl TryFrom<GridMsg> for GridJson {
    type Error = JsonError;

    fn try_from(msg: GridMsg) -> Result<Self, Self::Error> {
        Ok(match msg {
            GridMsg::RehydrateBegin => Self::RehydrateBegin,
            GridMsg::RehydrateEnd => Self::RehydrateEnd,
            GridMsg::RowDelta { rows } => {
                // One table per message, not per row: building one
                // zeroes its 24 KiB intern cache.
                let mut styles = StyleTable::new();
                Self::RowDelta {
                    rows: rows
                        .into_iter()
                        .map(|(row, payload)| {
                            Ok(RowJson {
                                row,
                                cells: row::from_payload(&payload, &mut styles)?,
                            })
                        })
                        .collect::<Result<Vec<_>, JsonError>>()?,
                }
            }
            GridMsg::Scrolled {
                region_top,
                region_bottom,
                n_rows,
                direction,
            } => Self::Scrolled {
                region_top,
                region_bottom,
                n_rows,
                direction: direction.into(),
            },
            GridMsg::Cluster { id, text } => Self::Cluster { id, text },
            GridMsg::Hyperlink { id, anchor, uri } => Self::Hyperlink { id, anchor, uri },
            GridMsg::CursorState {
                row,
                col,
                visible,
                style,
                blink,
            } => Self::CursorState {
                row,
                col,
                visible,
                style: style.into(),
                blink,
            },
            GridMsg::ViewportState {
                lines_from_bottom,
                max,
            } => Self::ViewportState {
                lines_from_bottom,
                max,
            },
            GridMsg::Size { dims } => Self::Size { dims: dims.into() },
            GridMsg::Title { value } => Self::Title { value },
            GridMsg::Cwd { value } => Self::Cwd { value },
            GridMsg::PromptMark {
                line,
                kind,
                exit_code,
            } => Self::PromptMark {
                line,
                kind: kind.into(),
                exit_code,
            },
            GridMsg::ThemeColor { channel, action } => Self::ThemeColor {
                channel: channel.into(),
                action: action.into(),
            },
            GridMsg::PaletteColor { index, action } => Self::PaletteColor {
                index,
                action: action.into(),
            },
            GridMsg::PaletteResetAll => Self::PaletteResetAll,
            GridMsg::PointerShape { name } => Self::PointerShape { name },
            GridMsg::KittyKbdFlags { flags } => Self::KittyKbdFlags {
                flags: flags.bits(),
            },
            GridMsg::ModeFlags {
                bracketed_paste,
                alt_screen,
                mouse_protocol,
                application_cursor,
                modify_other_keys,
                application_keypad,
                win32_input_mode,
                reverse_video,
            } => Self::ModeFlags {
                bracketed_paste,
                alt_screen,
                mouse_protocol: mouse_protocol.into(),
                application_cursor,
                modify_other_keys: modify_other_keys.into(),
                application_keypad,
                win32_input_mode,
                reverse_video,
            },
            GridMsg::Attention { source } => Self::Attention {
                source: source.into(),
            },
            GridMsg::ClipboardSet { write } => Self::ClipboardSet {
                write: write.into(),
            },
            GridMsg::CycleEnd => Self::CycleEnd,
        })
    }
}

impl TryFrom<GridJson> for GridMsg {
    type Error = JsonError;

    fn try_from(msg: GridJson) -> Result<Self, Self::Error> {
        Ok(match msg {
            GridJson::RehydrateBegin => Self::RehydrateBegin,
            GridJson::RehydrateEnd => Self::RehydrateEnd,
            GridJson::RowDelta { rows } => {
                let mut styles = StyleTable::new();
                Self::RowDelta {
                    rows: rows
                        .into_iter()
                        .map(|row| Ok((row.row, row::to_payload(&row.cells, &mut styles)?)))
                        .collect::<Result<Vec<(u16, RowPayload)>, JsonError>>()?,
                }
            }
            GridJson::Scrolled {
                region_top,
                region_bottom,
                n_rows,
                direction,
            } => Self::Scrolled {
                region_top,
                region_bottom,
                n_rows,
                direction: direction.into(),
            },
            GridJson::Cluster { id, text } => Self::Cluster { id, text },
            GridJson::Hyperlink { id, anchor, uri } => Self::Hyperlink { id, anchor, uri },
            GridJson::CursorState {
                row,
                col,
                visible,
                style,
                blink,
            } => Self::CursorState {
                row,
                col,
                visible,
                style: style.into(),
                blink,
            },
            GridJson::ViewportState {
                lines_from_bottom,
                max,
            } => Self::ViewportState {
                lines_from_bottom,
                max,
            },
            GridJson::Size { dims } => Self::Size { dims: dims.into() },
            GridJson::Title { value } => Self::Title { value },
            GridJson::Cwd { value } => Self::Cwd { value },
            GridJson::PromptMark {
                line,
                kind,
                exit_code,
            } => Self::PromptMark {
                line,
                kind: kind.into(),
                exit_code,
            },
            GridJson::ThemeColor { channel, action } => Self::ThemeColor {
                channel: channel.into(),
                action: action.into(),
            },
            GridJson::PaletteColor { index, action } => Self::PaletteColor {
                index,
                action: action.into(),
            },
            GridJson::PaletteResetAll => Self::PaletteResetAll,
            GridJson::PointerShape { name } => Self::PointerShape { name },
            GridJson::KittyKbdFlags { flags } => Self::KittyKbdFlags {
                flags: KittyKbdFlags::from_bits(flags).ok_or_else(|| {
                    JsonError::field("flags", "a reserved Kitty keyboard flag bit is set")
                })?,
            },
            GridJson::ModeFlags {
                bracketed_paste,
                alt_screen,
                mouse_protocol,
                application_cursor,
                modify_other_keys,
                application_keypad,
                win32_input_mode,
                reverse_video,
            } => Self::ModeFlags {
                bracketed_paste,
                alt_screen,
                mouse_protocol: mouse_protocol.into(),
                application_cursor,
                modify_other_keys: modify_other_keys.into(),
                application_keypad,
                win32_input_mode,
                reverse_video,
            },
            GridJson::Attention { source } => Self::Attention {
                source: source.into(),
            },
            GridJson::ClipboardSet { write } => Self::ClipboardSet {
                write: write.try_into()?,
            },
            GridJson::CycleEnd => Self::CycleEnd,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A selector this build does not define must not decode as the
    /// selection with that bit dropped, which would re-encode as a
    /// different clipboard write.
    #[test]
    fn a_reserved_clipboard_selector_bit_is_refused() {
        let write = ClipboardWriteJson {
            selection: CLIPBOARD_SELECTION_MAX + 1,
            data: Vec::new(),
        };
        assert!(matches!(
            ClipboardWrite::try_from(write),
            Err(JsonError::Field {
                field: "selection",
                ..
            })
        ));
    }

    #[test]
    fn every_defined_clipboard_selector_bit_is_accepted() {
        let write = ClipboardWriteJson {
            selection: CLIPBOARD_SELECTION_MAX,
            data: Vec::new(),
        };
        assert!(ClipboardWrite::try_from(write).is_ok());
    }

    #[test]
    fn a_reserved_kitty_keyboard_flag_bit_is_refused() {
        let msg = GridJson::KittyKbdFlags {
            flags: KITTY_KBD_FLAGS_MAX + 1,
        };
        assert!(matches!(
            GridMsg::try_from(msg),
            Err(JsonError::Field { field: "flags", .. })
        ));
    }

    #[test]
    fn every_defined_kitty_keyboard_flag_bit_is_accepted() {
        let msg = GridJson::KittyKbdFlags {
            flags: KITTY_KBD_FLAGS_MAX,
        };
        assert!(GridMsg::try_from(msg).is_ok());
    }
}

//! DTO for [`GridMsg`], the daemon→client grid diffs.

use super::DtoError;
use super::common::{Base64, GridDimsJson, RgbJson, ViewportAnchorJson};
use felis_grid::StyleTable;
use felis_grid::row_json::RowCellsJson;
use felis_protocol::RowPayload;
use felis_protocol::kitty_keyboard::KittyKbdFlags;
use felis_protocol::messages::{
    AttentionSource, ClipboardSelection, ClipboardWrite, CursorStyle, Directed as _, GridMsg,
    ModifyOtherKeys, MouseProtocol, PaletteAction, PromptKind, ScrollDirection, ThemeAction,
    ThemeChannel,
};

/// Every clipboard selector this build defines.
pub const CLIPBOARD_SELECTION_MAX: u8 = ClipboardSelection::all().bits();

/// Every Kitty keyboard progressive-enhancement bit this build defines.
pub const KITTY_KBD_FLAGS_MAX: u8 = KittyKbdFlags::all().bits();

json_dto! {
    /// One dirty row of a [`GridJson::RowDelta`]. The cells ride the
    /// row codec's structural form (`docs/reference/row-codec.md`),
    /// never the packed bytes the socket carries.
    #[non_exhaustive]
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
    #[non_exhaustive]
    pub struct ClipboardWriteJson {
        /// Bit 0 is the clipboard, bit 1 X11 PRIMARY
        /// (`felis.proto` `GridClipboardSet.selection`).
        #[cfg_attr(feature = "schema", schemars(range(max = CLIPBOARD_SELECTION_MAX)))]
        pub selection: u8,
        /// The decoded `OSC 52` payload.
        pub data: Base64,
    }

    /// Grid-family frames.
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
            /// Present exactly while `lines_from_bottom > 0`.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            anchor: Option<ViewportAnchorJson>,
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
            data: write.data.into(),
        }
    }
}

impl TryFrom<ClipboardWriteJson> for ClipboardWrite {
    type Error = DtoError;

    fn try_from(write: ClipboardWriteJson) -> Result<Self, DtoError> {
        Ok(Self {
            selection: ClipboardSelection::from_bits(write.selection).ok_or_else(|| {
                DtoError::field("selection", "a reserved clipboard selector bit is set")
            })?,
            data: write.data.0,
        })
    }
}

impl TryFrom<GridMsg> for GridJson {
    type Error = DtoError;

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
                                cells: RowCellsJson::from_payload(&payload, &mut styles)?,
                            })
                        })
                        .collect::<Result<Vec<_>, DtoError>>()?,
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
                anchor,
            } => Self::ViewportState {
                lines_from_bottom,
                max,
                anchor: anchor.map(Into::into),
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
            // A new variant is a new protocol version.
            ref outside @ GridMsg::CommandOutput { .. } => {
                return Err(DtoError::Outside(outside.variant()));
            }
        })
    }
}

impl TryFrom<GridJson> for GridMsg {
    type Error = DtoError;

    fn try_from(msg: GridJson) -> Result<Self, Self::Error> {
        Ok(match msg {
            GridJson::RehydrateBegin => Self::RehydrateBegin,
            GridJson::RehydrateEnd => Self::RehydrateEnd,
            GridJson::RowDelta { rows } => {
                let mut styles = StyleTable::new();
                Self::RowDelta {
                    rows: rows
                        .into_iter()
                        .map(|row| Ok((row.row, row.cells.to_payload(&mut styles)?)))
                        .collect::<Result<Vec<(u16, RowPayload)>, DtoError>>()?,
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
                anchor,
            } => Self::ViewportState {
                lines_from_bottom,
                max,
                anchor: anchor.map(Into::into),
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
                    DtoError::field("flags", "a reserved Kitty keyboard flag bit is set")
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

    #[test]
    fn a_command_output_is_refused_as_outside_the_format() {
        let msg = GridMsg::CommandOutput {
            text: "out".to_owned(),
            omitted: felis_protocol::messages::OmittedOutput::None,
        };
        assert!(matches!(GridJson::try_from(msg), Err(DtoError::Outside(_))));
    }

    /// A selector this build does not define must not decode as the
    /// selection with that bit dropped, which would re-encode as a
    /// different clipboard write.
    #[test]
    fn a_reserved_clipboard_selector_bit_is_refused() {
        let write = ClipboardWriteJson {
            selection: CLIPBOARD_SELECTION_MAX + 1,
            data: Base64::default(),
        };
        assert!(matches!(
            ClipboardWrite::try_from(write),
            Err(DtoError::Field {
                field: "selection",
                ..
            })
        ));
    }

    #[test]
    fn every_defined_clipboard_selector_bit_is_accepted() {
        let write = ClipboardWriteJson {
            selection: CLIPBOARD_SELECTION_MAX,
            data: Base64::default(),
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
            Err(DtoError::Field { field: "flags", .. })
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

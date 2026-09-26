//! `GridMsg` <-> wire.

use super::{WireError, decode_enum, dims_from_wire, narrow, rgb_from_wire, rgb_to_wire};
use crate::kitty_keyboard::KittyKbdFlags;
use crate::messages;
use crate::row::RowPayload;
use crate::wire::v1;

fn row_entry_to_wire((row, payload): &(u16, RowPayload)) -> v1::RowEntry {
    v1::RowEntry {
        row: u32::from(*row),
        packed_cells: payload.0.clone(),
    }
}

fn row_entry_from_wire(e: v1::RowEntry) -> Result<(u16, RowPayload), WireError> {
    Ok((narrow("RowEntry.row", e.row)?, RowPayload(e.packed_cells)))
}

data_free_enum!("AttentionSource", attention_source_to_i32, attention_source_from_i32, messages::AttentionSource, v1::AttentionSource, {
    Bell => Bell,
    Notification => Notification,
});

data_free_enum!("ScrollDirection", scroll_direction_to_i32, scroll_direction_from_i32, messages::ScrollDirection, v1::ScrollDirection, {
    Up => Up,
    Down => Down,
});

data_free_enum!("PromptKind", prompt_kind_to_i32, prompt_kind_from_i32, messages::PromptKind, v1::PromptKind, {
    PromptStart => PromptStart,
    InputStart => InputStart,
    OutputStart => OutputStart,
    CommandEnd => CommandEnd,
});

data_free_enum!("ThemeChannel", theme_channel_to_i32, theme_channel_from_i32, messages::ThemeChannel, v1::ThemeChannel, {
    Foreground => Foreground,
    Background => Background,
    Cursor => Cursor,
});

data_free_enum!("CursorStyle", cursor_style_to_i32, cursor_style_from_i32, messages::CursorStyle, v1::CursorStyle, {
    Block => Block,
    Underline => Underline,
    Bar => Bar,
});

data_free_enum!("MouseProtocol", mouse_protocol_to_i32, mouse_protocol_from_i32, messages::MouseProtocol, v1::MouseProtocol, {
    Off => Off,
    ButtonEvents => ButtonEvents,
    ButtonAndDrag => ButtonAndDrag,
    AnyMotion => AnyMotion,
});

data_free_enum!("ModifyOtherKeys", modify_other_keys_to_i32, modify_other_keys_from_i32, messages::ModifyOtherKeys, v1::ModifyOtherKeys, {
    Off => Off,
    Level1 => Level1,
    Level2 => Level2,
});

impl From<&messages::GridMsg> for v1::GridMsg {
    fn from(m: &messages::GridMsg) -> Self {
        use messages::GridMsg as G;
        use v1::grid_msg::Msg;
        let msg = match m {
            G::RehydrateBegin => Msg::RehydrateBegin(v1::GridRehydrateBegin {}),
            G::RehydrateEnd => Msg::RehydrateEnd(v1::GridRehydrateEnd {}),
            G::RowDelta { rows } => Msg::RowDelta(v1::GridRowDelta {
                rows: rows.iter().map(row_entry_to_wire).collect(),
            }),
            G::Scrolled {
                region_top,
                region_bottom,
                n_rows,
                direction,
            } => Msg::Scrolled(v1::GridScrolled {
                region_top: u32::from(*region_top),
                region_bottom: u32::from(*region_bottom),
                n_rows: u32::from(*n_rows),
                direction: scroll_direction_to_i32(*direction),
            }),
            G::Cluster { id, text } => Msg::Cluster(v1::GridCluster {
                id: *id,
                text: text.clone(),
            }),
            G::Hyperlink { id, anchor, uri } => Msg::Hyperlink(v1::GridHyperlink {
                id: u32::from(*id),
                anchor: anchor.clone(),
                uri: uri.clone(),
            }),
            G::CursorState {
                row,
                col,
                visible,
                style,
                blink,
            } => Msg::CursorState(v1::GridCursorState {
                row: u32::from(*row),
                col: u32::from(*col),
                visible: *visible,
                style: cursor_style_to_i32(*style),
                blink: *blink,
            }),
            G::ViewportState {
                lines_from_bottom,
                max,
            } => Msg::ViewportState(v1::GridViewportState {
                lines_from_bottom: *lines_from_bottom,
                max: *max,
            }),
            G::Size { dims } => Msg::Size(v1::GridSize {
                dims: Some((*dims).into()),
            }),
            G::Title { value } => Msg::Title(v1::GridTitle {
                value: value.clone(),
            }),
            G::Cwd { value } => Msg::Cwd(v1::GridCwd {
                value: value.clone(),
            }),
            G::PromptMark {
                line,
                kind,
                exit_code,
            } => Msg::PromptMark(v1::GridPromptMark {
                line: *line,
                kind: prompt_kind_to_i32(*kind),
                exit_code: *exit_code,
            }),
            G::ThemeColor { channel, action } => Msg::ThemeColor(v1::GridThemeColor {
                channel: theme_channel_to_i32(*channel),
                action: Some(match action {
                    messages::ThemeAction::Set { rgb } => {
                        v1::grid_theme_color::Action::Set(rgb_to_wire(*rgb))
                    }
                    messages::ThemeAction::Reset => {
                        v1::grid_theme_color::Action::Reset(v1::ThemeReset {})
                    }
                }),
            }),
            G::PaletteColor { index, action } => Msg::PaletteColor(v1::GridPaletteColor {
                index: u32::from(*index),
                action: Some(match action {
                    messages::PaletteAction::Set { rgb } => {
                        v1::grid_palette_color::Action::Set(rgb_to_wire(*rgb))
                    }
                    messages::PaletteAction::Reset => {
                        v1::grid_palette_color::Action::Reset(v1::PaletteReset {})
                    }
                }),
            }),
            G::PaletteResetAll => Msg::PaletteResetAll(v1::GridPaletteResetAll {}),
            G::CycleEnd => Msg::CycleEnd(v1::GridCycleEnd {}),
            G::PointerShape { name } => {
                Msg::PointerShape(v1::GridPointerShape { name: name.clone() })
            }
            G::KittyKbdFlags { flags } => Msg::KittyKbdFlags(v1::GridKittyKbdFlags {
                flags: u32::from(flags.bits()),
            }),
            G::ModeFlags {
                bracketed_paste,
                alt_screen,
                mouse_protocol,
                application_cursor,
                modify_other_keys,
                application_keypad,
                win32_input_mode,
                reverse_video,
            } => Msg::ModeFlags(v1::GridModeFlags {
                bracketed_paste: *bracketed_paste,
                alt_screen: *alt_screen,
                mouse_protocol: mouse_protocol_to_i32(*mouse_protocol),
                application_cursor: *application_cursor,
                modify_other_keys: modify_other_keys_to_i32(*modify_other_keys),
                application_keypad: *application_keypad,
                win32_input_mode: *win32_input_mode,
                reverse_video: *reverse_video,
            }),
            G::Attention { source } => Msg::Attention(v1::GridAttention {
                source: attention_source_to_i32(*source),
            }),
            G::ClipboardSet { write } => Msg::ClipboardSet(v1::GridClipboardSet {
                selection: u32::from(write.selection.bits()),
                data: write.data.clone(),
            }),
        };
        Self { msg: Some(msg) }
    }
}

impl TryFrom<v1::GridMsg> for messages::GridMsg {
    type Error = WireError;
    fn try_from(m: v1::GridMsg) -> Result<Self, Self::Error> {
        use v1::grid_msg::Msg;
        Ok(match m.msg.ok_or(WireError::MissingOneof("GridMsg.msg"))? {
            Msg::RehydrateBegin(_) => Self::RehydrateBegin,
            Msg::RehydrateEnd(_) => Self::RehydrateEnd,
            Msg::RowDelta(r) => Self::RowDelta {
                rows: r
                    .rows
                    .into_iter()
                    .map(row_entry_from_wire)
                    .collect::<Result<_, _>>()?,
            },
            Msg::Scrolled(s) => Self::Scrolled {
                region_top: narrow("Scrolled.region_top", s.region_top)?,
                region_bottom: narrow("Scrolled.region_bottom", s.region_bottom)?,
                n_rows: narrow("Scrolled.n_rows", s.n_rows)?,
                direction: scroll_direction_from_i32(s.direction)?,
            },
            Msg::Cluster(c) => Self::Cluster {
                id: c.id,
                text: c.text,
            },
            Msg::Hyperlink(h) => Self::Hyperlink {
                id: narrow("Hyperlink.id", h.id)?,
                anchor: h.anchor,
                uri: h.uri,
            },
            Msg::CursorState(c) => Self::CursorState {
                row: narrow("CursorState.row", c.row)?,
                col: narrow("CursorState.col", c.col)?,
                visible: c.visible,
                style: cursor_style_from_i32(c.style)?,
                blink: c.blink,
            },
            Msg::ViewportState(v) => Self::ViewportState {
                lines_from_bottom: v.lines_from_bottom,
                max: v.max,
            },
            Msg::Size(g) => Self::Size {
                dims: dims_from_wire(g.dims, "GridSize.dims")?,
            },
            Msg::Title(t) => Self::Title { value: t.value },
            Msg::Cwd(c) => Self::Cwd { value: c.value },
            Msg::PromptMark(p) => Self::PromptMark {
                line: p.line,
                kind: prompt_kind_from_i32(p.kind)?,
                exit_code: p.exit_code,
            },
            Msg::ThemeColor(t) => Self::ThemeColor {
                channel: theme_channel_from_i32(t.channel)?,
                action: match t
                    .action
                    .ok_or(WireError::MissingOneof("GridThemeColor.action"))?
                {
                    v1::grid_theme_color::Action::Set(rgb) => messages::ThemeAction::Set {
                        rgb: rgb_from_wire(rgb)?,
                    },
                    v1::grid_theme_color::Action::Reset(_) => messages::ThemeAction::Reset,
                },
            },
            Msg::PaletteColor(p) => Self::PaletteColor {
                index: narrow("PaletteColor.index", p.index)?,
                action: match p
                    .action
                    .ok_or(WireError::MissingOneof("GridPaletteColor.action"))?
                {
                    v1::grid_palette_color::Action::Set(rgb) => messages::PaletteAction::Set {
                        rgb: rgb_from_wire(rgb)?,
                    },
                    v1::grid_palette_color::Action::Reset(_) => messages::PaletteAction::Reset,
                },
            },
            Msg::PaletteResetAll(_) => Self::PaletteResetAll,
            Msg::CycleEnd(_) => Self::CycleEnd,
            Msg::PointerShape(p) => Self::PointerShape { name: p.name },
            Msg::KittyKbdFlags(k) => Self::KittyKbdFlags {
                flags: KittyKbdFlags::from_bits_truncate(narrow("KittyKbdFlags.flags", k.flags)?),
            },
            Msg::ModeFlags(f) => Self::ModeFlags {
                bracketed_paste: f.bracketed_paste,
                alt_screen: f.alt_screen,
                mouse_protocol: mouse_protocol_from_i32(f.mouse_protocol)?,
                application_cursor: f.application_cursor,
                modify_other_keys: modify_other_keys_from_i32(f.modify_other_keys)?,
                application_keypad: f.application_keypad,
                win32_input_mode: f.win32_input_mode,
                reverse_video: f.reverse_video,
            },
            Msg::Attention(a) => Self::Attention {
                source: attention_source_from_i32(a.source)?,
            },
            Msg::ClipboardSet(c) => Self::ClipboardSet {
                write: messages::ClipboardWrite {
                    selection: messages::ClipboardSelection::from_bits_truncate(narrow(
                        "ClipboardSet.selection",
                        c.selection,
                    )?),
                    data: c.data,
                },
            },
        })
    }
}

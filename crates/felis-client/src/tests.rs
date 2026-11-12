use felis_client_core::config::MAX_FONT_SIZE_LOGICAL_PX;
use felis_grid::ClipboardSelection;

use super::*;

const fn at(row: u16, col: u16) -> GridPos {
    GridPos { row, col }
}

const fn mode_flags(
    bracketed_paste: bool,
    alt_screen: bool,
    mouse_protocol: felis_protocol::messages::MouseProtocol,
) -> GridMsg {
    GridMsg::ModeFlags {
        bracketed_paste,
        alt_screen,
        mouse_protocol,
        application_cursor: false,
        modify_other_keys: felis_protocol::messages::ModifyOtherKeys::Off,
        application_keypad: false,
        win32_input_mode: false,
        reverse_video: false,
    }
}

fn clipboard_frame(selection: ClipboardSelection, data: &[u8], conn_gen: u64) -> AppEvent {
    AppEvent::GridFrame {
        msg: GridMsg::ClipboardSet {
            write: felis_protocol::messages::ClipboardWrite {
                selection,
                data: data.to_vec(),
            },
        },
        conn_gen,
    }
}

mod app_events;
mod cli;
mod ime;
mod lifecycle;
mod mouse;
mod render_config;
mod search;
mod selection;

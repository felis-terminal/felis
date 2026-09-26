//! What an applied [`GridMsg`] asks of the renderer beyond the shadow
//! screen, as data: a renderer trait here could not be implemented for
//! a frontend's renderer (`docs/explanation/architecture/overview.md`
//! "The felis-client-core / felis-client seam").

use felis_protocol::messages::{GridMsg, PaletteAction, ThemeAction, ThemeChannel};

/// A renderer-state change carried by one [`GridMsg`]. `rgb: None` drops
/// the override and restores the configured color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RendererEffect {
    /// OSC 10/11/12, and their 110/111/112 resets.
    ThemeOverride {
        channel: ThemeChannel,
        rgb: Option<(u8, u8, u8)>,
    },
    /// OSC 4 / 104 for one index: the override layer sits beside the
    /// configured palette it wins over.
    PaletteOverride {
        index: u8,
        rgb: Option<(u8, u8, u8)>,
    },
    /// OSC 104 with no index.
    ResetPalette,
    /// A rehydrate burst replays the incoming session's overrides but can
    /// never name the outgoing session's, so they are cleared first.
    ResetSessionColors,
    /// `?5` DECSCNM is decided at paint time: the flip must reach
    /// scrollback rows and cells no `RowDelta` re-sends.
    ReverseVideo(bool),
}

/// The renderer effect of `msg`, applied after the shadow screen has
/// absorbed it. Every variant and every `ModeFlags` field is named, so a
/// new one fails to build here until it is classified.
#[must_use]
pub const fn renderer_effect(msg: &GridMsg) -> Option<RendererEffect> {
    match msg {
        GridMsg::ThemeColor { channel, action } => Some(RendererEffect::ThemeOverride {
            channel: *channel,
            rgb: match action {
                ThemeAction::Set { rgb } => Some(*rgb),
                ThemeAction::Reset => None,
            },
        }),
        GridMsg::PaletteColor { index, action } => Some(RendererEffect::PaletteOverride {
            index: *index,
            rgb: match action {
                PaletteAction::Set { rgb } => Some(*rgb),
                PaletteAction::Reset => None,
            },
        }),
        GridMsg::PaletteResetAll => Some(RendererEffect::ResetPalette),
        GridMsg::RehydrateBegin => Some(RendererEffect::ResetSessionColors),
        GridMsg::ModeFlags {
            reverse_video,
            bracketed_paste: _,
            alt_screen: _,
            mouse_protocol: _,
            application_cursor: _,
            modify_other_keys: _,
            application_keypad: _,
            win32_input_mode: _,
        } => Some(RendererEffect::ReverseVideo(*reverse_video)),
        GridMsg::RehydrateEnd
        | GridMsg::RowDelta { .. }
        | GridMsg::Scrolled { .. }
        | GridMsg::Cluster { .. }
        | GridMsg::Hyperlink { .. }
        | GridMsg::CursorState { .. }
        | GridMsg::ViewportState { .. }
        | GridMsg::Size { .. }
        | GridMsg::Title { .. }
        | GridMsg::Cwd { .. }
        | GridMsg::PromptMark { .. }
        | GridMsg::PointerShape { .. }
        | GridMsg::KittyKbdFlags { .. }
        | GridMsg::Attention { .. }
        | GridMsg::ClipboardSet { .. }
        | GridMsg::CycleEnd => None,
    }
}

#[cfg(test)]
mod tests {
    use felis_protocol::messages::{ModifyOtherKeys, MouseProtocol};

    use super::*;

    fn mode_flags(reverse_video: bool) -> GridMsg {
        GridMsg::ModeFlags {
            bracketed_paste: true,
            alt_screen: true,
            mouse_protocol: MouseProtocol::AnyMotion,
            application_cursor: true,
            modify_other_keys: ModifyOtherKeys::Level2,
            application_keypad: true,
            win32_input_mode: true,
            reverse_video,
        }
    }

    #[test]
    fn a_theme_color_sets_or_drops_its_channel_override() {
        let set = GridMsg::ThemeColor {
            channel: ThemeChannel::Background,
            action: ThemeAction::Set { rgb: (1, 2, 3) },
        };
        let reset = GridMsg::ThemeColor {
            channel: ThemeChannel::Cursor,
            action: ThemeAction::Reset,
        };
        assert_eq!(
            renderer_effect(&set),
            Some(RendererEffect::ThemeOverride {
                channel: ThemeChannel::Background,
                rgb: Some((1, 2, 3)),
            })
        );
        assert_eq!(
            renderer_effect(&reset),
            Some(RendererEffect::ThemeOverride {
                channel: ThemeChannel::Cursor,
                rgb: None,
            })
        );
    }

    #[test]
    fn a_palette_color_sets_or_drops_its_index_override() {
        let set = GridMsg::PaletteColor {
            index: 9,
            action: PaletteAction::Set { rgb: (4, 5, 6) },
        };
        let reset = GridMsg::PaletteColor {
            index: 200,
            action: PaletteAction::Reset,
        };
        assert_eq!(
            renderer_effect(&set),
            Some(RendererEffect::PaletteOverride {
                index: 9,
                rgb: Some((4, 5, 6)),
            })
        );
        assert_eq!(
            renderer_effect(&reset),
            Some(RendererEffect::PaletteOverride {
                index: 200,
                rgb: None,
            })
        );
    }

    #[test]
    fn palette_reset_all_and_rehydrate_begin_clear_overrides() {
        assert_eq!(
            renderer_effect(&GridMsg::PaletteResetAll),
            Some(RendererEffect::ResetPalette)
        );
        assert_eq!(
            renderer_effect(&GridMsg::RehydrateBegin),
            Some(RendererEffect::ResetSessionColors)
        );
    }

    /// Every other mode flag is set, so only `reverse_video` can decide.
    #[test]
    fn mode_flags_carry_reverse_video_alone() {
        assert_eq!(
            renderer_effect(&mode_flags(true)),
            Some(RendererEffect::ReverseVideo(true))
        );
        assert_eq!(
            renderer_effect(&mode_flags(false)),
            Some(RendererEffect::ReverseVideo(false))
        );
    }

    #[test]
    fn messages_the_shadow_alone_consumes_have_no_effect() {
        assert_eq!(renderer_effect(&GridMsg::RehydrateEnd), None);
        assert_eq!(renderer_effect(&GridMsg::CycleEnd), None);
    }
}

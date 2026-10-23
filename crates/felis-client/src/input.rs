//! The winit keyboard adapter: winit key events become a
//! [`KeyPress`](crate::window_input::KeyPress). The mapping lives here so
//! `felis-client-core` stays free of any windowing crate
//! (docs/explanation/architecture/overview.md
//! "Workspace: the crate-boundary decision record").

use felis_protocol::messages::{Key, KeyEvent, KeyEventKind, KeyLocation};
use winit::event::ElementState;
use winit::keyboard::{
    Key as WinitKey, KeyLocation as WinitKeyLocation, NamedKey as WinitNamedKey,
};

use crate::window_input::PressedKey;
use crate::winit_keys;

pub(crate) const fn event_kind(state: ElementState, repeat: bool) -> KeyEventKind {
    match state {
        ElementState::Released => KeyEventKind::Release,
        ElementState::Pressed if repeat => KeyEventKind::Repeat,
        ElementState::Pressed => KeyEventKind::Press,
    }
}

/// Whether forwarding `event` returns a scrolled-back viewport to the
/// live grid. A release, or a textless [`Key::Other`] (a bare modifier,
/// a dead key), would otherwise yank the view away mid-selection: Cmd
/// alone snaps before Cmd+C can copy.
pub(crate) fn snaps_to_live(event: &KeyEvent) -> bool {
    event.kind != KeyEventKind::Release && (event.key != Key::Other || event.text.is_some())
}

/// Unrecognized named keys and every non-character key (dead keys, media
/// keys) collapse to [`PressedKey::Other`], which the daemon routes to
/// the `text` passthrough.
pub(crate) fn pressed_key(key: &WinitKey) -> PressedKey {
    match key {
        WinitKey::Named(
            WinitNamedKey::Shift
            | WinitNamedKey::Control
            | WinitNamedKey::Alt
            | WinitNamedKey::AltGraph
            | WinitNamedKey::Super
            | WinitNamedKey::Meta
            | WinitNamedKey::CapsLock,
        ) => PressedKey::Modifier,
        WinitKey::Named(named) => {
            winit_keys::named(*named).map_or(PressedKey::Other, PressedKey::Named)
        }
        WinitKey::Character(s) => PressedKey::Character(s.as_str().to_owned()),
        _ => PressedKey::Other,
    }
}

pub(crate) const fn location(loc: WinitKeyLocation) -> KeyLocation {
    match loc {
        WinitKeyLocation::Standard => KeyLocation::Standard,
        WinitKeyLocation::Left => KeyLocation::Left,
        WinitKeyLocation::Right => KeyLocation::Right,
        WinitKeyLocation::Numpad => KeyLocation::Numpad,
    }
}

#[cfg(test)]
mod tests {
    use felis_client_core::{FKey, Modifiers, NamedKey};

    use super::*;
    use crate::window_input::KeyPress;

    #[test]
    fn maps_named_character_and_other_keys() {
        assert_eq!(
            pressed_key(&WinitKey::Named(WinitNamedKey::Enter)),
            PressedKey::Named(NamedKey::Enter),
        );
        assert_eq!(
            pressed_key(&WinitKey::Named(WinitNamedKey::F7)),
            PressedKey::Named(NamedKey::F(FKey::lit(7))),
        );
        assert_eq!(
            pressed_key(&WinitKey::Character("a".into())),
            PressedKey::Character("a".into()),
        );
        assert_eq!(
            pressed_key(&WinitKey::Named(WinitNamedKey::Shift)),
            PressedKey::Modifier
        );
        assert_eq!(pressed_key(&WinitKey::Dead(Some('´'))), PressedKey::Other);
    }

    #[test]
    fn a_held_key_reports_repeat_and_a_release_reports_release() {
        assert_eq!(
            event_kind(ElementState::Pressed, false),
            KeyEventKind::Press
        );
        assert_eq!(
            event_kind(ElementState::Pressed, true),
            KeyEventKind::Repeat
        );
        // winit sets `repeat` on the synthesized repeats only, so a
        // release never carries it; the kind must not depend on it.
        assert_eq!(
            event_kind(ElementState::Released, true),
            KeyEventKind::Release
        );
    }

    #[test]
    fn a_key_event_carries_the_facts_the_daemon_encodes_from() {
        let event = KeyPress {
            key: pressed_key(&WinitKey::Character("a".into())),
            text: Some("a".into()),
            mods: Modifiers::CONTROL,
            kind: KeyEventKind::Press,
            location: location(WinitKeyLocation::Numpad),
        }
        .to_wire();
        assert_eq!(event.key, Key::Character("a".into()));
        assert_eq!(event.text.as_deref(), Some("a"));
        assert!(event.mods.control_key());
        assert_eq!(event.location, KeyLocation::Numpad);
    }

    /// A bare modifier must reach the wire as `Other` so the daemon
    /// defers to the (absent) text passthrough.
    #[test]
    fn a_bare_modifier_reaches_the_wire_as_other() {
        assert_eq!(
            event(
                &WinitKey::Named(WinitNamedKey::Shift),
                None,
                KeyEventKind::Press
            )
            .key,
            Key::Other
        );
    }

    fn event(key: &WinitKey, text: Option<&str>, kind: KeyEventKind) -> KeyEvent {
        KeyPress {
            key: pressed_key(key),
            text: text.map(str::to_owned),
            mods: Modifiers::empty(),
            kind,
            location: KeyLocation::Standard,
        }
        .to_wire()
    }

    #[test]
    fn a_pressed_or_repeated_key_snaps_to_live() {
        let a = WinitKey::Character("a".into());
        let enter = WinitKey::Named(WinitNamedKey::Enter);
        assert!(snaps_to_live(&event(&a, Some("a"), KeyEventKind::Press)));
        assert!(snaps_to_live(&event(&a, Some("a"), KeyEventKind::Repeat)));
        assert!(snaps_to_live(&event(&enter, None, KeyEventKind::Press)));
    }

    #[test]
    fn a_bare_modifier_does_not_snap_to_live() {
        for modifier in [
            WinitNamedKey::Shift,
            WinitNamedKey::Control,
            WinitNamedKey::Alt,
            WinitNamedKey::Super,
            WinitNamedKey::Meta,
        ] {
            let key = WinitKey::Named(modifier);
            assert!(
                !snaps_to_live(&event(&key, None, KeyEventKind::Press)),
                "{modifier:?} press"
            );
        }
    }

    #[test]
    fn a_release_does_not_snap_to_live() {
        let a = WinitKey::Character("a".into());
        assert!(!snaps_to_live(&event(&a, Some("a"), KeyEventKind::Release)));
    }

    #[test]
    fn a_dead_key_snaps_to_live_only_once_it_composes_text() {
        let dead = WinitKey::Dead(Some('´'));
        assert!(!snaps_to_live(&event(&dead, None, KeyEventKind::Press)));
        assert!(snaps_to_live(&event(&dead, Some("é"), KeyEventKind::Press)));
    }
}

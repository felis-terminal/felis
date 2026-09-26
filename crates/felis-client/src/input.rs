//! Keyboard → wire translation: the winit adapter over the structured
//! key event `InputMsg::Key` carries. The winit→wire mapping lives here
//! so `felis-client-core` stays free of any windowing crate
//! (docs/explanation/architecture/overview.md
//! "Workspace: the crate-boundary decision record").

use felis_protocol::messages::{Key, KeyEvent, KeyEventKind, KeyLocation};
use winit::event::ElementState;
use winit::keyboard::{Key as WinitKey, KeyLocation as WinitKeyLocation, ModifiersState};

use crate::winit_keys;

/// The wire form of one winit key event. The bytes it reaches the child
/// as are the daemon's to decide, against the keyboard modes it owns
/// (docs/explanation/input.md "Keyboard").
pub(crate) fn key_event(
    key: &WinitKey,
    text: Option<&str>,
    mods: ModifiersState,
    kind: KeyEventKind,
    location: WinitKeyLocation,
) -> KeyEvent {
    KeyEvent {
        key: to_key(key),
        text: text.map(str::to_owned),
        mods: winit_keys::mods(mods),
        kind,
        location: to_location(location),
    }
}

pub(crate) const fn event_kind(state: ElementState, repeat: bool) -> KeyEventKind {
    match state {
        ElementState::Released => KeyEventKind::Release,
        ElementState::Pressed if repeat => KeyEventKind::Repeat,
        ElementState::Pressed => KeyEventKind::Press,
    }
}

/// Unrecognized named keys and every non-character key (dead keys, bare
/// modifiers, media keys) collapse to [`Key::Other`], which the daemon
/// routes to the `text` passthrough.
fn to_key(key: &WinitKey) -> Key {
    match key {
        WinitKey::Named(named) => winit_keys::named(*named).map_or(Key::Other, Key::Named),
        WinitKey::Character(s) => Key::Character(s.as_str().to_owned()),
        _ => Key::Other,
    }
}

const fn to_location(loc: WinitKeyLocation) -> KeyLocation {
    match loc {
        WinitKeyLocation::Standard => KeyLocation::Standard,
        WinitKeyLocation::Left => KeyLocation::Left,
        WinitKeyLocation::Right => KeyLocation::Right,
        WinitKeyLocation::Numpad => KeyLocation::Numpad,
    }
}

#[cfg(test)]
mod tests {
    use felis_client_core::{FKey, NamedKey};
    use winit::keyboard::NamedKey as WinitNamedKey;

    use super::*;

    #[test]
    fn maps_named_character_and_other_keys() {
        assert_eq!(
            to_key(&WinitKey::Named(WinitNamedKey::Enter)),
            Key::Named(NamedKey::Enter),
        );
        assert_eq!(
            to_key(&WinitKey::Named(WinitNamedKey::F7)),
            Key::Named(NamedKey::F(FKey::lit(7))),
        );
        assert_eq!(
            to_key(&WinitKey::Character("a".into())),
            Key::Character("a".into()),
        );
        // A bare modifier must collapse to `Other` so the daemon defers
        // to the (absent) text passthrough.
        assert_eq!(to_key(&WinitKey::Named(WinitNamedKey::Shift)), Key::Other);
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
        let event = key_event(
            &WinitKey::Character("a".into()),
            Some("a"),
            ModifiersState::CONTROL,
            KeyEventKind::Press,
            WinitKeyLocation::Numpad,
        );
        assert_eq!(event.key, Key::Character("a".into()));
        assert_eq!(event.text.as_deref(), Some("a"));
        assert!(event.mods.control_key());
        assert_eq!(event.location, KeyLocation::Numpad);
    }
}

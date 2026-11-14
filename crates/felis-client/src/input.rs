//! The winit keyboard adapter: winit key events become a
//! [`KeyPress`](crate::window_input::KeyPress). The mapping lives here so
//! `felis-client-core` stays free of any windowing crate
//! (docs/explanation/architecture/overview.md
//! "Workspace: the crate-boundary decision record").

use std::collections::HashMap;

use felis_client_core::{Modifiers, NamedKey};
use felis_protocol::messages::{Key, KeyEvent, KeyEventKind, KeyLocation};
use winit::event::{ElementState, KeyEvent as WinitKeyEvent};
use winit::keyboard::{
    Key as WinitKey, KeyCode, KeyLocation as WinitKeyLocation, NamedKey as WinitNamedKey,
    NativeKey, PhysicalKey,
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
/// live grid. A release, a textless [`Key::Other`] (a dead key), or a
/// modifier, lock, media or volume key would otherwise yank the view
/// away mid-selection: Cmd alone snaps before Cmd+C can copy.
pub(crate) fn snaps_to_live(event: &KeyEvent) -> bool {
    event.kind != KeyEventKind::Release
        && match &event.key {
            Key::Other => event.text.is_some(),
            Key::Named(named) => !is_bystander(*named),
            Key::Character(_) => true,
        }
}

const fn is_bystander(named: NamedKey) -> bool {
    modifier(named)
        || matches!(
            named,
            NamedKey::CapsLock
                | NamedKey::NumLock
                | NamedKey::ScrollLock
                | NamedKey::MediaPlay
                | NamedKey::MediaPause
                | NamedKey::MediaPlayPause
                | NamedKey::MediaStop
                | NamedKey::MediaFastForward
                | NamedKey::MediaRewind
                | NamedKey::MediaTrackNext
                | NamedKey::MediaTrackPrevious
                | NamedKey::MediaRecord
                | NamedKey::VolumeDown
                | NamedKey::VolumeUp
                | NamedKey::VolumeMute
        )
}

const fn modifier(named: NamedKey) -> bool {
    matches!(
        named,
        NamedKey::Shift
            | NamedKey::Control
            | NamedKey::Alt
            | NamedKey::Super
            | NamedKey::Hyper
            | NamedKey::Meta
            | NamedKey::IsoLevel3Shift
            | NamedKey::IsoLevel5Shift
    )
}

/// XKB keysyms winit leaves unidentified.
const XKB_BEGIN: u32 = 0xff58;
const XKB_KP_BEGIN: u32 = 0xff9d;
const XKB_META_L: u32 = 0xffe7;
const XKB_META_R: u32 = 0xffe8;
const XKB_ISO_LEVEL5_SHIFT: u32 = 0xfe11;

/// Dead keys and named keys felis does not bridge collapse to
/// [`PressedKey::Other`], which the daemon routes to the `text`
/// passthrough.
pub(crate) fn pressed_key(key: &WinitKey, physical: PhysicalKey) -> PressedKey {
    let named = match key {
        WinitKey::Named(WinitNamedKey::AltGraph) => NamedKey::IsoLevel3Shift,
        // Windows names the keypad's middle key with Num Lock off Clear.
        WinitKey::Named(WinitNamedKey::Clear)
            if physical == PhysicalKey::Code(KeyCode::Numpad5) =>
        {
            NamedKey::Begin
        }
        WinitKey::Named(named) => match winit_keys::named(*named) {
            Some(named) => named,
            None => return PressedKey::Other,
        },
        WinitKey::Character(s) => return PressedKey::Character(s.as_str().to_owned()),
        WinitKey::Unidentified(NativeKey::Xkb(keysym)) => match *keysym {
            XKB_BEGIN | XKB_KP_BEGIN => NamedKey::Begin,
            XKB_META_L | XKB_META_R => NamedKey::Meta,
            XKB_ISO_LEVEL5_SHIFT => NamedKey::IsoLevel5Shift,
            _ => return PressedKey::Other,
        },
        _ => return PressedKey::Other,
    };
    if modifier(named) || matches!(named, NamedKey::CapsLock) {
        PressedKey::Modifier(named)
    } else {
        PressedKey::Named(named)
    }
}

/// The modifier keys held, by physical key, so a modifier key's own
/// event reports its bit as the Kitty spec's "Modifiers" section asks:
/// set by the press, and kept by the release while the other side's key
/// is still held. winit's modifier state lags the key event that changes
/// it, and does not say which side is down.
#[derive(Debug, Default)]
pub(crate) struct HeldModifiers(HashMap<PhysicalKey, NamedKey>);

impl HeldModifiers {
    /// The modifiers `key`'s own event reports, given the ones held
    /// before it.
    pub(crate) fn track(
        &mut self,
        physical: PhysicalKey,
        key: &PressedKey,
        kind: KeyEventKind,
        mods: Modifiers,
    ) -> Modifiers {
        let PressedKey::Modifier(named) = *key else {
            return mods;
        };
        if kind == KeyEventKind::Release {
            self.0.remove(&physical);
        } else {
            self.0.insert(physical, named);
        }
        let bit = match named {
            NamedKey::Shift => Modifiers::SHIFT,
            NamedKey::Control => Modifiers::CONTROL,
            NamedKey::Alt => Modifiers::ALT,
            NamedKey::Super => Modifiers::SUPER,
            _ => return mods,
        };
        let mut mods = mods;
        mods.set(bit, self.0.values().any(|held| *held == named));
        mods
    }

    /// Focus loss: the releases of keys still held go to another window.
    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }

    fn level_shifted(&self) -> bool {
        self.0
            .values()
            .any(|held| matches!(held, NamedKey::IsoLevel3Shift | NamedKey::IsoLevel5Shift))
    }
}

/// The key a character keystroke names with no modifier applied, sent
/// only where it can differ from the key lowercased: while Shift is held
/// and neither an `AltGr` key nor Option composition chose the character.
pub(crate) fn unshifted(
    without_modifiers: &WinitKey,
    mods: Modifiers,
    composed: bool,
    held: &HeldModifiers,
) -> Option<char> {
    if !mods.shift_key() || composed || held.level_shifted() {
        return None;
    }
    let WinitKey::Character(s) = without_modifiers else {
        return None;
    };
    let mut chars = s.chars();
    let c = chars.next()?;
    chars.next().is_none().then_some(c)
}

#[cfg(any(
    target_os = "windows",
    target_os = "macos",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
pub(crate) fn key_without_modifiers(event: &WinitKeyEvent) -> WinitKey {
    use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
    event.key_without_modifiers()
}

#[cfg(not(any(
    target_os = "windows",
    target_os = "macos",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)))]
pub(crate) const fn key_without_modifiers(_: &WinitKeyEvent) -> WinitKey {
    WinitKey::Unidentified(winit::keyboard::NativeKey::Unidentified)
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

    fn pressed(key: &WinitKey) -> PressedKey {
        pressed_key(key, PhysicalKey::Code(KeyCode::KeyA))
    }

    #[test]
    fn maps_named_character_and_other_keys() {
        assert_eq!(
            pressed(&WinitKey::Named(WinitNamedKey::Enter)),
            PressedKey::Named(NamedKey::Enter),
        );
        assert_eq!(
            pressed(&WinitKey::Named(WinitNamedKey::F7)),
            PressedKey::Named(NamedKey::F(FKey::lit(7))),
        );
        assert_eq!(
            pressed(&WinitKey::Character("a".into())),
            PressedKey::Character("a".into()),
        );
        assert_eq!(
            pressed(&WinitKey::Named(WinitNamedKey::Shift)),
            PressedKey::Modifier(NamedKey::Shift)
        );
        assert_eq!(pressed(&WinitKey::Dead(Some('´'))), PressedKey::Other);
    }

    #[test]
    fn keys_winit_leaves_unidentified_or_names_otherwise_reach_their_named_key() {
        let xkb = |keysym| pressed(&WinitKey::Unidentified(NativeKey::Xkb(keysym)));
        assert_eq!(xkb(XKB_KP_BEGIN), PressedKey::Named(NamedKey::Begin));
        assert_eq!(xkb(XKB_META_L), PressedKey::Modifier(NamedKey::Meta));
        assert_eq!(
            xkb(XKB_ISO_LEVEL5_SHIFT),
            PressedKey::Modifier(NamedKey::IsoLevel5Shift)
        );
        assert_eq!(
            pressed(&WinitKey::Named(WinitNamedKey::AltGraph)),
            PressedKey::Modifier(NamedKey::IsoLevel3Shift)
        );
        let clear = WinitKey::Named(WinitNamedKey::Clear);
        assert_eq!(
            pressed_key(&clear, PhysicalKey::Code(KeyCode::Numpad5)),
            PressedKey::Named(NamedKey::Begin)
        );
        assert_eq!(pressed(&clear), PressedKey::Other);
    }

    /// Kitty spec "Modifiers": releasing one of two held Control keys
    /// keeps the ctrl bit; releasing the last clears it.
    #[test]
    fn a_modifier_keys_own_event_reports_its_bit_while_any_key_of_it_is_held() {
        let mut held = HeldModifiers::default();
        let left = PhysicalKey::Code(KeyCode::ControlLeft);
        let right = PhysicalKey::Code(KeyCode::ControlRight);
        let ctrl = PressedKey::Modifier(NamedKey::Control);
        let none = Modifiers::empty();
        assert_eq!(
            held.track(left, &ctrl, KeyEventKind::Press, none),
            Modifiers::CONTROL
        );
        assert_eq!(
            held.track(right, &ctrl, KeyEventKind::Press, Modifiers::CONTROL),
            Modifiers::CONTROL
        );
        assert_eq!(
            held.track(left, &ctrl, KeyEventKind::Release, Modifiers::CONTROL),
            Modifiers::CONTROL
        );
        assert_eq!(
            held.track(right, &ctrl, KeyEventKind::Release, Modifiers::CONTROL),
            none
        );
        let a = PressedKey::Character("a".into());
        assert_eq!(
            held.track(left, &a, KeyEventKind::Press, Modifiers::SHIFT),
            Modifiers::SHIFT
        );
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
            key: pressed(&WinitKey::Character("a".into())),
            text: Some("a".into()),
            mods: Modifiers::CONTROL,
            kind: KeyEventKind::Press,
            location: location(WinitKeyLocation::Numpad),
            unshifted: Some('3'),
            base_layout: Some('3'),
        }
        .to_wire();
        assert_eq!(event.key, Key::Character("a".into()));
        assert_eq!(event.text.as_deref(), Some("a"));
        assert!(event.mods.control_key());
        assert_eq!(event.location, KeyLocation::Numpad);
        assert_eq!((event.unshifted, event.base_layout), (Some('3'), Some('3')));
    }

    #[test]
    fn the_unshifted_key_rides_only_on_a_shifted_keystroke() {
        let three = WinitKey::Character("3".into());
        let none = HeldModifiers::default();
        assert_eq!(unshifted(&three, Modifiers::SHIFT, false, &none), Some('3'));
        assert_eq!(unshifted(&three, Modifiers::CONTROL, false, &none), None);
    }

    #[test]
    fn no_unshifted_key_rides_on_a_composed_or_altgr_keystroke() {
        let l = WinitKey::Character("l".into());
        let mut altgr = HeldModifiers::default();
        let right_alt = PhysicalKey::Code(KeyCode::AltRight);
        let altgr_key = PressedKey::Modifier(NamedKey::IsoLevel3Shift);
        let shift = Modifiers::SHIFT;
        assert_eq!(unshifted(&l, shift, true, &altgr), None);
        altgr.track(right_alt, &altgr_key, KeyEventKind::Press, shift);
        assert_eq!(unshifted(&l, shift, false, &altgr), None);
        altgr.track(right_alt, &altgr_key, KeyEventKind::Release, shift);
        assert_eq!(unshifted(&l, shift, false, &altgr), Some('l'));
    }

    /// The daemon reports a modifier key under the Kitty report-all
    /// flag, by its side.
    #[test]
    fn a_bare_modifier_reaches_the_wire_as_its_named_key() {
        assert_eq!(
            event(
                &WinitKey::Named(WinitNamedKey::Shift),
                None,
                KeyEventKind::Press
            )
            .key,
            Key::Named(NamedKey::Shift)
        );
    }

    fn event(key: &WinitKey, text: Option<&str>, kind: KeyEventKind) -> KeyEvent {
        KeyPress {
            key: pressed(key),
            text: text.map(str::to_owned),
            mods: Modifiers::empty(),
            kind,
            location: KeyLocation::Standard,
            unshifted: None,
            base_layout: None,
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
            WinitNamedKey::CapsLock,
            WinitNamedKey::NumLock,
            WinitNamedKey::AudioVolumeUp,
            WinitNamedKey::MediaPlayPause,
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

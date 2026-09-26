//! winit `Key` + `ModifiersState` to `felis_client_core::Chord` adapter.
//!
//! Normalizes ASCII letters to lowercase + Shift (`docs/reference/keybindings.md`) so
//! bindings have one canonical spelling regardless of how winit reports case.

use felis_client_core::keymap::{Chord, KeyCode, Modifiers};
use winit::keyboard::{Key, ModifiersState};

use crate::winit_keys;

/// `None` when the key is outside the chord-bindable subset (composite
/// IME strings, dead keys, unidentified keys, unhandled named keys); the
/// input-encoding path keeps owning those.
#[must_use]
pub(crate) fn winit_to_chord(key: &Key, mods: ModifiersState) -> Option<Chord> {
    let key_code = winit_key_to_keycode(key)?;
    let (key_code, lifted_shift) = lift_uppercase_letter(key_code);
    let mut chord_mods = winit_keys::mods(mods);
    if lifted_shift {
        chord_mods |= Modifiers::SHIFT;
    }
    Some(Chord::with_mods(chord_mods, key_code))
}

fn winit_key_to_keycode(key: &Key) -> Option<KeyCode> {
    match key {
        Key::Named(named) => winit_keys::named(*named).map(KeyCode::Named),
        Key::Character(s) => {
            // Composite IME strings and dead-key chains (more than one
            // codepoint) route back to the encoder path.
            let mut chars = s.chars();
            let first = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            Some(KeyCode::Character(first.to_string()))
        }
        _ => None,
    }
}

/// Winit reports `Key::Character("V")` for Shift+v on US layouts; the
/// default keymap and a `Chord`'s canonical Display form carry the
/// lowercase letter and Shift separately. The `bool` is `true` when the
/// letter was lowercased.
fn lift_uppercase_letter(key: KeyCode) -> (KeyCode, bool) {
    match key {
        KeyCode::Character(s) if s.len() == 1 => {
            let bytes = s.as_bytes();
            if bytes[0].is_ascii_uppercase() {
                let lower = (bytes[0]).to_ascii_lowercase();
                let lower_char = char::from(lower);
                (KeyCode::Character(lower_char.to_string()), true)
            } else {
                (KeyCode::Character(s), false)
            }
        }
        other => (other, false),
    }
}

#[cfg(test)]
mod tests {
    use winit::keyboard::NamedKey;

    use super::*;

    fn ctrl_shift() -> ModifiersState {
        ModifiersState::CONTROL | ModifiersState::SHIFT
    }

    #[test]
    fn lowercase_letter_passes_through_unchanged() {
        let chord = winit_to_chord(
            &Key::Character("v".into()),
            ModifiersState::CONTROL | ModifiersState::SHIFT,
        )
        .unwrap();
        assert_eq!(chord, "ctrl+shift+v".parse().unwrap());
    }

    #[test]
    fn uppercase_letter_is_lowercased_and_shift_kept_in_modifier_set() {
        // winit reports "V" + Shift for Shift+v on US layouts.
        let chord = winit_to_chord(&Key::Character("V".into()), ctrl_shift()).unwrap();
        assert_eq!(chord, "ctrl+shift+v".parse().unwrap());
    }

    #[test]
    fn uppercase_letter_under_caps_lock_without_shift_still_lowercases() {
        // Caps Lock makes winit report "A" with no Shift modifier; the lift
        // sets SHIFT whenever it lowercases (it doesn't know about Caps
        // Lock), which beats silently confusing Shift-bound chords with
        // Caps-Lock-typed letters.
        let chord = winit_to_chord(&Key::Character("A".into()), ModifiersState::empty()).unwrap();
        assert_eq!(chord, "shift+a".parse().unwrap());
    }

    #[test]
    fn non_letter_character_is_not_lifted() {
        // Lifting non-letters would need the shifted-character
        // relationship per keyboard layout.
        let chord = winit_to_chord(&Key::Character("+".into()), ctrl_shift()).unwrap();
        assert_eq!(chord, "ctrl+shift++".parse().unwrap());

        let chord = winit_to_chord(&Key::Character("=".into()), ctrl_shift()).unwrap();
        assert_eq!(chord, "ctrl+shift+=".parse().unwrap());
    }

    #[test]
    fn named_keys_map_one_to_one() {
        // A future winit version can't rename a variant unnoticed.
        let cases: &[(NamedKey, &str)] = &[
            (NamedKey::Enter, "enter"),
            (NamedKey::Tab, "tab"),
            (NamedKey::Escape, "escape"),
            (NamedKey::Space, "space"),
            (NamedKey::Backspace, "backspace"),
            (NamedKey::Insert, "insert"),
            (NamedKey::Delete, "delete"),
            (NamedKey::Home, "home"),
            (NamedKey::End, "end"),
            (NamedKey::PageUp, "page_up"),
            (NamedKey::PageDown, "page_down"),
            (NamedKey::ArrowUp, "up"),
            (NamedKey::ArrowDown, "down"),
            (NamedKey::ArrowLeft, "left"),
            (NamedKey::ArrowRight, "right"),
            (NamedKey::F1, "f1"),
            (NamedKey::F12, "f12"),
        ];
        for (winit_key, expect) in cases {
            let chord = winit_to_chord(&Key::Named(*winit_key), ModifiersState::empty())
                .unwrap_or_else(|| panic!("expected {winit_key:?} to map"));
            assert_eq!(chord.to_string(), *expect, "winit {winit_key:?}");
        }
    }

    #[test]
    fn named_keys_outside_subset_return_none() {
        // Modifier keys are never the key part of a chord; `None` punts to
        // the encoder path, which ignores standalone modifier presses.
        assert!(winit_to_chord(&Key::Named(NamedKey::Shift), ModifiersState::empty()).is_none());
    }

    #[test]
    fn modifier_bitset_is_reproduced_faithfully() {
        // The dispatcher relies on `mods.contains(Modifiers::SUPER)`
        // matching `ModifiersState::SUPER` end-to-end.
        let chord = winit_to_chord(
            &Key::Character("a".into()),
            ModifiersState::CONTROL
                | ModifiersState::SHIFT
                | ModifiersState::ALT
                | ModifiersState::SUPER,
        )
        .unwrap();
        assert_eq!(chord, "ctrl+shift+alt+super+a".parse().unwrap());
    }

    #[test]
    fn multi_codepoint_character_returns_none() {
        // Composite IME strings or dead-key chains are not chord-bindable.
        assert!(winit_to_chord(&Key::Character("ab".into()), ModifiersState::empty()).is_none(),);
    }

    #[test]
    fn non_ascii_character_passes_through() {
        // The daemon-side keymap dispatcher needs a platform-free Chord; it
        // doesn't care which keyboard layout produced it.
        let chord = winit_to_chord(&Key::Character("あ".into()), ModifiersState::empty()).unwrap();
        assert_eq!(chord.to_string(), "あ");
    }
}

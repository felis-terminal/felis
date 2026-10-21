//! macOS Option composition (`window.macos_option_as_alt`): which key,
//! text, and modifiers a keystroke carries into keymap matching and onto
//! the wire, and how a release finds the press it ends.

use std::collections::HashMap;

use felis_protocol::messages::KeyEventKind;
use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey, PhysicalKey};

/// What keymap matching and the wire event both see for one key event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Keystroke {
    pub(crate) key: Key,
    pub(crate) text: Option<String>,
    pub(crate) mods: ModifiersState,
}

/// The press a held key started, keyed by its physical key so repeats and
/// the release report what the press reported, whatever winit re-derives
/// from the modifiers held by then.
#[derive(Debug)]
pub(crate) struct KeyPresses {
    track: bool,
    compose_option: bool,
    held: HashMap<PhysicalKey, Held>,
}

#[derive(Debug)]
struct Held {
    key: Key,
    text: Option<String>,
    composed: bool,
}

impl Held {
    fn stroke(&self, mods: ModifiersState, text: Option<String>) -> Keystroke {
        let mut mods = mods;
        if self.composed {
            mods.remove(ModifiersState::ALT);
        }
        Keystroke {
            key: self.key.clone(),
            text,
            mods,
        }
    }
}

impl KeyPresses {
    /// Only macOS composes characters from Option while reporting it as
    /// Alt, so every other platform forwards events untouched.
    pub(crate) fn for_platform(option_as_alt: bool) -> Self {
        let macos = cfg!(target_os = "macos");
        Self::new(macos, macos && !option_as_alt)
    }

    pub(crate) fn new(track: bool, compose_option: bool) -> Self {
        Self {
            track,
            compose_option,
            held: HashMap::new(),
        }
    }

    pub(crate) const fn set_option_as_alt(&mut self, option_as_alt: bool) {
        self.compose_option = self.track && !option_as_alt;
    }

    /// Focus loss: the releases of keys still held go to another window.
    pub(crate) fn clear(&mut self) {
        self.held.clear();
    }

    /// `None` drops the event: a release whose press the program never
    /// saw, such as the key winit consumed into an IME commit.
    pub(crate) fn resolve(
        &mut self,
        physical: PhysicalKey,
        kind: KeyEventKind,
        key: &Key,
        text: Option<&str>,
        text_with_all_modifiers: Option<&str>,
        mods: ModifiersState,
    ) -> Option<Keystroke> {
        if !self.track {
            return Some(Keystroke {
                key: key.clone(),
                text: text.map(str::to_owned),
                mods,
            });
        }
        if kind == KeyEventKind::Release {
            return self
                .held
                .remove(&physical)
                .map(|held| held.stroke(mods, None));
        }
        if kind == KeyEventKind::Repeat
            && let Some(held) = self.held.get(&physical)
        {
            return Some(if held.composed {
                held.stroke(mods, held.text.clone())
            } else {
                Keystroke {
                    key: key.clone(),
                    text: text.map(str::to_owned),
                    mods,
                }
            });
        }
        let composed = if self.compose_option {
            option_composed_key(key, text_with_all_modifiers, mods)
        } else {
            None
        };
        let held = match composed {
            Some(key) => Held {
                text: key.to_text().map(str::to_owned),
                key,
                composed: true,
            },
            None => Held {
                key: key.clone(),
                text: text.map(str::to_owned),
                composed: false,
            },
        };
        let stroke = held.stroke(mods, held.text.clone());
        self.held.insert(physical, held);
        Some(stroke)
    }
}

/// The key a press reports when the OS composed a character from Option;
/// `None` keeps Alt. Option+Space's composed character (NBSP on US
/// layouts) survives only in `text_with_all_modifiers`.
fn option_composed_key(
    key: &Key,
    text_with_all_modifiers: Option<&str>,
    mods: ModifiersState,
) -> Option<Key> {
    if !mods.alt_key() || mods.control_key() || mods.super_key() {
        return None;
    }
    match key {
        Key::Character(_) => Some(key.clone()),
        Key::Named(NamedKey::Space) => text_with_all_modifiers
            .filter(|composed| !composed.is_empty() && *composed != " ")
            .map(|composed| Key::Character(composed.into())),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn text_with_all_modifiers(event: &KeyEvent) -> Option<&str> {
    use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
    event.text_with_all_modifiers()
}

#[cfg(not(target_os = "macos"))]
pub(crate) const fn text_with_all_modifiers(_: &KeyEvent) -> Option<&str> {
    None
}

#[cfg(test)]
mod tests {
    use winit::keyboard::KeyCode;

    use super::*;

    const ALT: ModifiersState = ModifiersState::ALT;
    const NBSP: &str = "\u{a0}";

    fn composing() -> KeyPresses {
        KeyPresses::new(true, true)
    }

    fn ch(s: &str) -> Key {
        Key::Character(s.into())
    }

    fn space() -> Key {
        Key::Named(NamedKey::Space)
    }

    fn stroke(key: Key, text: Option<&str>, mods: ModifiersState) -> Keystroke {
        Keystroke {
            key,
            text: text.map(str::to_owned),
            mods,
        }
    }

    fn press(
        presses: &mut KeyPresses,
        code: KeyCode,
        key: &Key,
        text: Option<&str>,
        all: Option<&str>,
        mods: ModifiersState,
    ) -> Option<Keystroke> {
        presses.resolve(
            PhysicalKey::Code(code),
            KeyEventKind::Press,
            key,
            text,
            all,
            mods,
        )
    }

    fn release(
        presses: &mut KeyPresses,
        code: KeyCode,
        key: &Key,
        mods: ModifiersState,
    ) -> Option<Keystroke> {
        presses.resolve(
            PhysicalKey::Code(code),
            KeyEventKind::Release,
            key,
            None,
            None,
            mods,
        )
    }

    #[test]
    fn option_composed_character_drops_alt() {
        let at = ch("@");
        let got = press(
            &mut composing(),
            KeyCode::KeyL,
            &at,
            Some("@"),
            Some("@"),
            ALT,
        );
        assert_eq!(
            got,
            Some(stroke(ch("@"), Some("@"), ModifiersState::empty()))
        );
    }

    #[test]
    fn option_composed_character_keeps_shift() {
        let mods = ALT | ModifiersState::SHIFT;
        let got = press(
            &mut composing(),
            KeyCode::KeyL,
            &ch("Ò"),
            Some("Ò"),
            Some("Ò"),
            mods,
        );
        assert_eq!(got.unwrap().mods, ModifiersState::SHIFT);
    }

    #[test]
    fn option_space_forwards_the_composed_character_without_alt() {
        let got = press(
            &mut composing(),
            KeyCode::Space,
            &space(),
            Some(" "),
            Some(NBSP),
            ALT,
        );
        assert_eq!(
            got,
            Some(stroke(ch(NBSP), Some(NBSP), ModifiersState::empty()))
        );
    }

    #[test]
    fn option_space_that_composes_a_plain_space_keeps_alt() {
        for all in [Some(" "), Some(""), None] {
            let got = press(
                &mut composing(),
                KeyCode::Space,
                &space(),
                Some(" "),
                all,
                ALT,
            );
            assert_eq!(got, Some(stroke(space(), Some(" "), ALT)), "{all:?}");
        }
    }

    #[test]
    fn option_with_other_named_keys_keeps_alt() {
        for (code, named) in [
            (KeyCode::ArrowLeft, NamedKey::ArrowLeft),
            (KeyCode::Enter, NamedKey::Enter),
            (KeyCode::Backspace, NamedKey::Backspace),
        ] {
            let key = Key::Named(named);
            let got = press(&mut composing(), code, &key, None, None, ALT);
            assert_eq!(got, Some(stroke(key, None, ALT)), "{named:?}");
        }
    }

    #[test]
    fn option_under_control_or_command_keeps_alt() {
        for extra in [ModifiersState::CONTROL, ModifiersState::SUPER] {
            let mods = ALT | extra;
            let got = press(
                &mut composing(),
                KeyCode::KeyB,
                &ch("b"),
                Some("b"),
                Some("b"),
                mods,
            );
            assert_eq!(got, Some(stroke(ch("b"), Some("b"), mods)), "{extra:?}");
            let got = press(
                &mut composing(),
                KeyCode::Space,
                &space(),
                Some(" "),
                Some(NBSP),
                mods,
            );
            assert_eq!(got, Some(stroke(space(), Some(" "), mods)), "{extra:?}");
        }
    }

    #[test]
    fn a_character_without_option_is_untouched() {
        let mods = ModifiersState::CONTROL;
        let got = press(
            &mut composing(),
            KeyCode::KeyC,
            &ch("c"),
            Some("c"),
            Some("\u{3}"),
            mods,
        );
        assert_eq!(got, Some(stroke(ch("c"), Some("c"), mods)));
    }

    #[test]
    fn option_as_alt_keeps_alt_on_every_key() {
        let mut presses = KeyPresses::new(true, false);
        let got = press(
            &mut presses,
            KeyCode::KeyB,
            &ch("b"),
            Some("b"),
            Some("b"),
            ALT,
        );
        assert_eq!(got, Some(stroke(ch("b"), Some("b"), ALT)));
        let got = press(
            &mut presses,
            KeyCode::Space,
            &space(),
            Some(" "),
            Some(" "),
            ALT,
        );
        assert_eq!(got, Some(stroke(space(), Some(" "), ALT)));
    }

    #[test]
    fn set_option_as_alt_switches_the_decision_for_later_presses() {
        let mut presses = composing();
        presses.set_option_as_alt(true);
        let got = press(
            &mut presses,
            KeyCode::KeyB,
            &ch("b"),
            Some("b"),
            Some("b"),
            ALT,
        );
        assert_eq!(got.unwrap().mods, ALT);
        presses.set_option_as_alt(false);
        let got = press(
            &mut presses,
            KeyCode::KeyL,
            &ch("@"),
            Some("@"),
            Some("@"),
            ALT,
        );
        assert_eq!(got.unwrap().mods, ModifiersState::empty());
    }

    #[test]
    fn a_repeat_reports_the_press_key_text_and_decision() {
        let mut presses = composing();
        press(
            &mut presses,
            KeyCode::Space,
            &space(),
            Some(" "),
            Some(NBSP),
            ALT,
        );
        let got = presses.resolve(
            PhysicalKey::Code(KeyCode::Space),
            KeyEventKind::Repeat,
            &space(),
            Some(" "),
            Some(NBSP),
            ALT,
        );
        assert_eq!(
            got,
            Some(stroke(ch(NBSP), Some(NBSP), ModifiersState::empty()))
        );
    }

    #[test]
    fn a_repeat_of_an_uncomposed_press_reports_its_own_key_and_text() {
        let mut presses = composing();
        let none = ModifiersState::empty();
        press(
            &mut presses,
            KeyCode::KeyA,
            &ch("a"),
            Some("a"),
            Some("a"),
            none,
        );
        let shift = ModifiersState::SHIFT;
        let got = presses.resolve(
            PhysicalKey::Code(KeyCode::KeyA),
            KeyEventKind::Repeat,
            &ch("A"),
            Some("A"),
            Some("A"),
            shift,
        );
        assert_eq!(got, Some(stroke(ch("A"), Some("A"), shift)));
    }

    #[test]
    fn a_release_after_option_lets_go_reports_the_pressed_key() {
        let mut presses = composing();
        press(
            &mut presses,
            KeyCode::KeyL,
            &ch("@"),
            Some("@"),
            Some("@"),
            ALT,
        );
        let got = release(
            &mut presses,
            KeyCode::KeyL,
            &ch("l"),
            ModifiersState::empty(),
        );
        assert_eq!(got, Some(stroke(ch("@"), None, ModifiersState::empty())));
    }

    #[test]
    fn a_release_of_a_composed_press_drops_alt_while_option_is_held() {
        let mut presses = composing();
        press(
            &mut presses,
            KeyCode::KeyL,
            &ch("@"),
            Some("@"),
            Some("@"),
            ALT,
        );
        let got = release(&mut presses, KeyCode::KeyL, &ch("@"), ALT);
        assert_eq!(got, Some(stroke(ch("@"), None, ModifiersState::empty())));
    }

    #[test]
    fn a_release_of_an_alt_press_keeps_the_current_modifiers() {
        let mut presses = composing();
        let left = Key::Named(NamedKey::ArrowLeft);
        press(&mut presses, KeyCode::ArrowLeft, &left, None, None, ALT);
        let got = release(&mut presses, KeyCode::ArrowLeft, &left, ALT);
        assert_eq!(got, Some(stroke(left, None, ALT)));
    }

    #[test]
    fn a_release_ends_its_press() {
        let mut presses = composing();
        press(
            &mut presses,
            KeyCode::KeyA,
            &ch("a"),
            Some("a"),
            Some("a"),
            ModifiersState::empty(),
        );
        assert!(
            release(
                &mut presses,
                KeyCode::KeyA,
                &ch("a"),
                ModifiersState::empty()
            )
            .is_some()
        );
        assert_eq!(
            release(
                &mut presses,
                KeyCode::KeyA,
                &ch("a"),
                ModifiersState::empty()
            ),
            None
        );
    }

    #[test]
    fn a_release_without_a_press_is_dropped() {
        let got = release(
            &mut composing(),
            KeyCode::KeyE,
            &ch("e"),
            ModifiersState::empty(),
        );
        assert_eq!(got, None);
    }

    #[test]
    fn focus_loss_forgets_held_presses() {
        let mut presses = composing();
        press(
            &mut presses,
            KeyCode::KeyA,
            &ch("a"),
            Some("a"),
            Some("a"),
            ModifiersState::empty(),
        );
        presses.clear();
        assert_eq!(
            release(
                &mut presses,
                KeyCode::KeyA,
                &ch("a"),
                ModifiersState::empty()
            ),
            None
        );
    }

    #[test]
    fn untracked_platforms_forward_every_event_untouched() {
        let mut presses = KeyPresses::new(false, false);
        let got = press(
            &mut presses,
            KeyCode::KeyL,
            &ch("@"),
            Some("@"),
            Some("@"),
            ALT,
        );
        assert_eq!(got, Some(stroke(ch("@"), Some("@"), ALT)));
        let got = release(&mut presses, KeyCode::KeyE, &ch("e"), ALT);
        assert_eq!(got, Some(stroke(ch("e"), None, ALT)));
    }
}

//! Per-platform default keymap (docs/reference/keybindings.md).
//!
//! Letter chords are stored lowercase: the winit -> `Chord` adapter in
//! `felis-client` lowercases ASCII letters and folds Shift back into the
//! modifier set.

use felis_protocol::messages::PromptJump;

use crate::action::{Action, ClipboardScope, FontSizeStep, IpcAction, ScrollStep};
use crate::keymap::chord::{Chord, KeyCode, Modifiers, NamedKey};
use crate::keymap::map::Keymap;

impl Keymap {
    #[must_use]
    pub fn default_for_platform() -> Self {
        let mut km = Self::common_linux_style();
        if cfg!(target_os = "macos") {
            km.apply_macos_additions();
        }
        km
    }

    fn common_linux_style() -> Self {
        let mut km = Self::new();

        let ctrl_shift = Modifiers::CONTROL | Modifiers::SHIFT;

        km.insert(
            chord(ctrl_shift, KeyCode::Character("v".into())),
            Action::Paste {
                from: ClipboardScope::System,
            },
        );
        km.insert(
            chord(ctrl_shift, KeyCode::Character("c".into())),
            Action::Copy {
                what: ClipboardScope::System,
            },
        );
        km.insert(
            chord(ctrl_shift, KeyCode::Character("r".into())),
            Action::Reload,
        );
        km.insert(
            chord(ctrl_shift, KeyCode::Character("f".into())),
            Action::Ipc(IpcAction::OpenScrollbackSearch),
        );
        // `pipe`, `run`, and the session-switch tokens ship no default
        // chord (docs/reference/keybindings.md "Unbound by default").

        // Both zoom spellings: Shift produces the shifted character on
        // some layouts (US: `Shift+=` is `+`) and not others (DE).
        for sym in ["=", "+"] {
            km.insert(
                chord(ctrl_shift, KeyCode::Character(sym.into())),
                Action::FontSize(FontSizeStep::Increase),
            );
        }
        for sym in ["-", "_"] {
            km.insert(
                chord(ctrl_shift, KeyCode::Character(sym.into())),
                Action::FontSize(FontSizeStep::Decrease),
            );
        }
        for sym in ["0", ")"] {
            km.insert(
                chord(ctrl_shift, KeyCode::Character(sym.into())),
                Action::FontSize(FontSizeStep::Reset),
            );
        }

        // PRIMARY paste: xterm / urxvt / alacritty / kitty all bind it.
        km.insert(
            chord(Modifiers::SHIFT, KeyCode::Named(NamedKey::Insert)),
            Action::Paste {
                from: ClipboardScope::Primary,
            },
        );

        // Bare `End` is gated by the dispatcher on viewport state, so vim
        // / pagers still see `kend` at the live view.
        km.insert(
            chord(Modifiers::CONTROL, KeyCode::Named(NamedKey::Home)),
            Action::Scroll(ScrollStep::Home),
        );
        km.insert(
            chord(Modifiers::CONTROL, KeyCode::Named(NamedKey::End)),
            Action::Scroll(ScrollStep::End),
        );
        km.insert(
            chord(Modifiers::empty(), KeyCode::Named(NamedKey::End)),
            Action::Scroll(ScrollStep::End),
        );
        km.insert(
            chord(Modifiers::SHIFT, KeyCode::Named(NamedKey::PageUp)),
            Action::Scroll(ScrollStep::HalfPageUp),
        );
        km.insert(
            chord(Modifiers::SHIFT, KeyCode::Named(NamedKey::PageDown)),
            Action::Scroll(ScrollStep::HalfPageDown),
        );
        // Prompt jump stays on `Ctrl+Shift` on macOS too: `Cmd+Shift+Z`
        // is the system redo.
        km.insert(
            chord(ctrl_shift, KeyCode::Character("z".into())),
            Action::ScrollToPrompt(PromptJump::Previous),
        );
        km.insert(
            chord(ctrl_shift, KeyCode::Character("x".into())),
            Action::ScrollToPrompt(PromptJump::Next),
        );

        km
    }

    fn apply_macos_additions(&mut self) {
        // Shift is don't-care for the `Cmd` chords, so both shapes are
        // inserted.
        let cmd = Modifiers::SUPER;
        let cmd_shift = Modifiers::SUPER | Modifiers::SHIFT;

        let modal_letters: &[(&str, Action)] = &[
            (
                "v",
                Action::Paste {
                    from: ClipboardScope::System,
                },
            ),
            (
                "c",
                Action::Copy {
                    what: ClipboardScope::System,
                },
            ),
            ("r", Action::Reload),
            ("f", Action::Ipc(IpcAction::OpenScrollbackSearch)),
        ];
        for (letter, action) in modal_letters {
            self.insert(
                chord(cmd, KeyCode::Character((*letter).into())),
                action.clone(),
            );
            self.insert(
                chord(cmd_shift, KeyCode::Character((*letter).into())),
                action.clone(),
            );
        }

        let zoom: &[(&[&str], Action)] = &[
            (&["=", "+"], Action::FontSize(FontSizeStep::Increase)),
            (&["-", "_"], Action::FontSize(FontSizeStep::Decrease)),
            (&["0", ")"], Action::FontSize(FontSizeStep::Reset)),
        ];
        for (syms, action) in zoom {
            for sym in *syms {
                self.insert(
                    chord(cmd, KeyCode::Character((*sym).into())),
                    action.clone(),
                );
                self.insert(
                    chord(cmd_shift, KeyCode::Character((*sym).into())),
                    action.clone(),
                );
            }
        }
    }
}

const fn chord(mods: Modifiers, key: KeyCode) -> Chord {
    Chord::with_mods(mods, key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Chord {
        s.parse().unwrap()
    }

    #[test]
    fn linux_style_baseline_binds_every_documented_chord() {
        let km = Keymap::common_linux_style();

        assert_eq!(
            km.resolve(&parse("ctrl+shift+v")),
            Some(&Action::Paste {
                from: ClipboardScope::System,
            }),
        );
        assert_eq!(
            km.resolve(&parse("ctrl+shift+c")),
            Some(&Action::Copy {
                what: ClipboardScope::System,
            }),
        );
        assert_eq!(km.resolve(&parse("ctrl+shift+r")), Some(&Action::Reload),);
        assert_eq!(
            km.resolve(&parse("ctrl+shift+f")),
            Some(&Action::Ipc(IpcAction::OpenScrollbackSearch)),
        );
        // kitty's `show_scrollback` chord: `pipe` has no default.
        assert_eq!(km.resolve(&parse("ctrl+shift+h")), None);

        for sym in ["=", "+"] {
            let chord = format!("ctrl+shift+{sym}");
            assert_eq!(
                km.resolve(&parse(&chord)),
                Some(&Action::FontSize(FontSizeStep::Increase)),
                "zoom-in: {chord}",
            );
        }
        for sym in ["-", "_"] {
            let chord = format!("ctrl+shift+{sym}");
            assert_eq!(
                km.resolve(&parse(&chord)),
                Some(&Action::FontSize(FontSizeStep::Decrease)),
                "zoom-out: {chord}",
            );
        }
        for sym in ["0", ")"] {
            let chord = format!("ctrl+shift+{sym}");
            assert_eq!(
                km.resolve(&parse(&chord)),
                Some(&Action::FontSize(FontSizeStep::Reset)),
                "zoom-reset: {chord}",
            );
        }

        assert_eq!(
            km.resolve(&parse("shift+insert")),
            Some(&Action::Paste {
                from: ClipboardScope::Primary,
            }),
        );

        assert_eq!(
            km.resolve(&parse("ctrl+home")),
            Some(&Action::Scroll(ScrollStep::Home)),
        );
        assert_eq!(
            km.resolve(&parse("ctrl+end")),
            Some(&Action::Scroll(ScrollStep::End)),
        );
        assert_eq!(
            km.resolve(&parse("shift+page_up")),
            Some(&Action::Scroll(ScrollStep::HalfPageUp)),
        );
        assert_eq!(
            km.resolve(&parse("shift+page_down")),
            Some(&Action::Scroll(ScrollStep::HalfPageDown)),
        );
        assert_eq!(
            km.resolve(&parse("ctrl+shift+z")),
            Some(&Action::ScrollToPrompt(PromptJump::Previous)),
        );
        assert_eq!(
            km.resolve(&parse("ctrl+shift+x")),
            Some(&Action::ScrollToPrompt(PromptJump::Next)),
        );
    }

    #[test]
    fn unbound_chords_do_not_resolve() {
        let km = Keymap::common_linux_style();

        assert!(km.resolve(&parse("ctrl+v")).is_none());
        assert!(km.resolve(&parse("a")).is_none());
    }

    /// The dispatcher's viewport gate assumes this binding exists.
    #[test]
    fn bare_end_is_in_the_default_table() {
        let km = Keymap::common_linux_style();
        assert_eq!(
            km.resolve(&parse("end")),
            Some(&Action::Scroll(ScrollStep::End)),
        );
    }

    /// Pinned via `apply_macos_additions` rather than
    /// `default_for_platform` so coverage is uniform across CI hosts.
    #[test]
    fn macos_additions_layer_in_cmd_variants() {
        let mut km = Keymap::common_linux_style();
        km.apply_macos_additions();

        let modal_letters: &[(&str, Action)] = &[
            (
                "v",
                Action::Paste {
                    from: ClipboardScope::System,
                },
            ),
            (
                "c",
                Action::Copy {
                    what: ClipboardScope::System,
                },
            ),
            ("r", Action::Reload),
            ("f", Action::Ipc(IpcAction::OpenScrollbackSearch)),
        ];
        for (letter, action) in modal_letters {
            assert_eq!(
                km.resolve(&parse(&format!("super+{letter}"))),
                Some(action),
                "super+{letter} action on macOS",
            );
            assert_eq!(
                km.resolve(&parse(&format!("super+shift+{letter}"))),
                Some(action),
                "super+shift+{letter} action on macOS",
            );
        }

        let zoom: &[(&str, Action)] = &[
            ("=", Action::FontSize(FontSizeStep::Increase)),
            ("+", Action::FontSize(FontSizeStep::Increase)),
            ("-", Action::FontSize(FontSizeStep::Decrease)),
            ("_", Action::FontSize(FontSizeStep::Decrease)),
            ("0", Action::FontSize(FontSizeStep::Reset)),
            (")", Action::FontSize(FontSizeStep::Reset)),
        ];
        for (sym, action) in zoom {
            assert_eq!(
                km.resolve(&parse(&format!("super+{sym}"))),
                Some(action),
                "super+{sym} action on macOS",
            );
            assert_eq!(
                km.resolve(&parse(&format!("super+shift+{sym}"))),
                Some(action),
                "super+shift+{sym} action on macOS",
            );
        }
    }

    /// macOS accepts both Ctrl+Shift+letter and Cmd+letter.
    #[test]
    fn macos_additions_do_not_drop_ctrl_shift_baseline() {
        let mut km = Keymap::common_linux_style();
        km.apply_macos_additions();
        assert_eq!(
            km.resolve(&parse("ctrl+shift+v")),
            Some(&Action::Paste {
                from: ClipboardScope::System,
            }),
        );
        assert_eq!(km.resolve(&parse("ctrl+shift+r")), Some(&Action::Reload),);
    }

    #[test]
    fn default_for_platform_picks_the_right_table() {
        let km = Keymap::default_for_platform();

        assert!(km.resolve(&parse("ctrl+shift+v")).is_some());

        let has_cmd = km.resolve(&parse("super+v")).is_some();
        assert_eq!(
            has_cmd,
            cfg!(target_os = "macos"),
            "macOS additions must layer iff target_os=macos",
        );
    }
}

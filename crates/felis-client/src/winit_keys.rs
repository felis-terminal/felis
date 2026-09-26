//! Shared winit → `felis-client-core` key conversions, so
//! [`keymap_adapter`](crate::keymap_adapter) and [`input`](crate::input)
//! cannot drift on which keys / modifiers they recognize.

use felis_client_core::{FKey, Modifiers, NamedKey};
use winit::keyboard::{ModifiersState, NamedKey as WinitNamedKey};

/// `None` for keys neither bridge recognizes. F1–F35 is the full band
/// winit reports; adding media keys is an arm here plus
/// `felis_client_core::keymap::NamedKey`.
pub(crate) const fn named(named: WinitNamedKey) -> Option<NamedKey> {
    Some(match named {
        WinitNamedKey::Enter => NamedKey::Enter,
        WinitNamedKey::Tab => NamedKey::Tab,
        WinitNamedKey::Escape => NamedKey::Escape,
        WinitNamedKey::Space => NamedKey::Space,
        WinitNamedKey::Backspace => NamedKey::Backspace,
        WinitNamedKey::Insert => NamedKey::Insert,
        WinitNamedKey::Delete => NamedKey::Delete,
        WinitNamedKey::Home => NamedKey::Home,
        WinitNamedKey::End => NamedKey::End,
        WinitNamedKey::PageUp => NamedKey::PageUp,
        WinitNamedKey::PageDown => NamedKey::PageDown,
        WinitNamedKey::ArrowUp => NamedKey::ArrowUp,
        WinitNamedKey::ArrowDown => NamedKey::ArrowDown,
        WinitNamedKey::ArrowLeft => NamedKey::ArrowLeft,
        WinitNamedKey::ArrowRight => NamedKey::ArrowRight,
        WinitNamedKey::F1 => NamedKey::F(FKey::lit(1)),
        WinitNamedKey::F2 => NamedKey::F(FKey::lit(2)),
        WinitNamedKey::F3 => NamedKey::F(FKey::lit(3)),
        WinitNamedKey::F4 => NamedKey::F(FKey::lit(4)),
        WinitNamedKey::F5 => NamedKey::F(FKey::lit(5)),
        WinitNamedKey::F6 => NamedKey::F(FKey::lit(6)),
        WinitNamedKey::F7 => NamedKey::F(FKey::lit(7)),
        WinitNamedKey::F8 => NamedKey::F(FKey::lit(8)),
        WinitNamedKey::F9 => NamedKey::F(FKey::lit(9)),
        WinitNamedKey::F10 => NamedKey::F(FKey::lit(10)),
        WinitNamedKey::F11 => NamedKey::F(FKey::lit(11)),
        WinitNamedKey::F12 => NamedKey::F(FKey::lit(12)),
        WinitNamedKey::F13 => NamedKey::F(FKey::lit(13)),
        WinitNamedKey::F14 => NamedKey::F(FKey::lit(14)),
        WinitNamedKey::F15 => NamedKey::F(FKey::lit(15)),
        WinitNamedKey::F16 => NamedKey::F(FKey::lit(16)),
        WinitNamedKey::F17 => NamedKey::F(FKey::lit(17)),
        WinitNamedKey::F18 => NamedKey::F(FKey::lit(18)),
        WinitNamedKey::F19 => NamedKey::F(FKey::lit(19)),
        WinitNamedKey::F20 => NamedKey::F(FKey::lit(20)),
        WinitNamedKey::F21 => NamedKey::F(FKey::lit(21)),
        WinitNamedKey::F22 => NamedKey::F(FKey::lit(22)),
        WinitNamedKey::F23 => NamedKey::F(FKey::lit(23)),
        WinitNamedKey::F24 => NamedKey::F(FKey::lit(24)),
        WinitNamedKey::F25 => NamedKey::F(FKey::lit(25)),
        WinitNamedKey::F26 => NamedKey::F(FKey::lit(26)),
        WinitNamedKey::F27 => NamedKey::F(FKey::lit(27)),
        WinitNamedKey::F28 => NamedKey::F(FKey::lit(28)),
        WinitNamedKey::F29 => NamedKey::F(FKey::lit(29)),
        WinitNamedKey::F30 => NamedKey::F(FKey::lit(30)),
        WinitNamedKey::F31 => NamedKey::F(FKey::lit(31)),
        WinitNamedKey::F32 => NamedKey::F(FKey::lit(32)),
        WinitNamedKey::F33 => NamedKey::F(FKey::lit(33)),
        WinitNamedKey::F34 => NamedKey::F(FKey::lit(34)),
        WinitNamedKey::F35 => NamedKey::F(FKey::lit(35)),
        _ => return None,
    })
}

pub(crate) fn mods(mods: ModifiersState) -> Modifiers {
    let mut out = Modifiers::empty();
    out.set(Modifiers::CONTROL, mods.control_key());
    out.set(Modifiers::SHIFT, mods.shift_key());
    out.set(Modifiers::ALT, mods.alt_key());
    out.set(Modifiers::SUPER, mods.super_key());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_maps_the_full_f1_through_f35_band() {
        // The whole F1..=F35 band must reach the keymap so a chord like
        // `f13` bound in config fires.
        let expected: &[(WinitNamedKey, u8)] = &[
            (WinitNamedKey::F1, 1),
            (WinitNamedKey::F2, 2),
            (WinitNamedKey::F3, 3),
            (WinitNamedKey::F4, 4),
            (WinitNamedKey::F5, 5),
            (WinitNamedKey::F6, 6),
            (WinitNamedKey::F7, 7),
            (WinitNamedKey::F8, 8),
            (WinitNamedKey::F9, 9),
            (WinitNamedKey::F10, 10),
            (WinitNamedKey::F11, 11),
            (WinitNamedKey::F12, 12),
            (WinitNamedKey::F13, 13),
            (WinitNamedKey::F14, 14),
            (WinitNamedKey::F15, 15),
            (WinitNamedKey::F16, 16),
            (WinitNamedKey::F17, 17),
            (WinitNamedKey::F18, 18),
            (WinitNamedKey::F19, 19),
            (WinitNamedKey::F20, 20),
            (WinitNamedKey::F21, 21),
            (WinitNamedKey::F22, 22),
            (WinitNamedKey::F23, 23),
            (WinitNamedKey::F24, 24),
            (WinitNamedKey::F25, 25),
            (WinitNamedKey::F26, 26),
            (WinitNamedKey::F27, 27),
            (WinitNamedKey::F28, 28),
            (WinitNamedKey::F29, 29),
            (WinitNamedKey::F30, 30),
            (WinitNamedKey::F31, 31),
            (WinitNamedKey::F32, 32),
            (WinitNamedKey::F33, 33),
            (WinitNamedKey::F34, 34),
            (WinitNamedKey::F35, 35),
        ];
        for &(winit_key, index) in expected {
            assert_eq!(
                named(winit_key),
                Some(NamedKey::F(FKey::lit(index))),
                "winit F{index} must map to NamedKey::F({index})",
            );
        }
    }

    #[test]
    fn named_returns_none_outside_the_shared_subset() {
        // Bare modifiers and media keys aren't in either bridge's set.
        assert_eq!(named(WinitNamedKey::Shift), None);
    }

    #[test]
    fn mods_reproduces_every_bit() {
        let m = mods(
            ModifiersState::CONTROL
                | ModifiersState::SHIFT
                | ModifiersState::ALT
                | ModifiersState::SUPER,
        );
        assert_eq!(m, Modifiers::all());
        assert_eq!(mods(ModifiersState::empty()), Modifiers::empty());
    }
}

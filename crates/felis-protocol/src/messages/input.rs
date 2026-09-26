//! [`InputMsg`]: client→daemon terminal input (kind 1), with the
//! mouse- and key-event vocabulary it carries.

use std::fmt;

use bitflags::bitflags;
use serde::{Deserialize, Serialize};

use super::{ArmMeta, CorrelationClass, Directed, Direction, ModeSet};

use super::RequestedDims;

bitflags! {
    /// Modifier keys held when an input event fires.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct InputMods: u8 {
        const SHIFT = 1 << 0;
        /// Alt / option / meta modifier.
        const ALT   = 1 << 1;
        const CTRL  = 1 << 2;
    }
}

bitflags! {
    /// Modifiers held when a key event fires. Wider than [`InputMods`]
    /// by `SUPER`, which the Kitty keyboard protocol reports and the
    /// xterm mouse encoding has no bit for.
    #[derive(
        Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
    )]
    pub struct KeyMods: u8 {
        const CTRL  = 0b0001;
        const SHIFT = 0b0010;
        const ALT   = 0b0100;
        /// Command on macOS, Super / Win elsewhere.
        const SUPER = 0b1000;
        /// The spelling the keymap chord grammar uses.
        const CONTROL = Self::CTRL.bits();
    }
}

impl KeyMods {
    #[must_use]
    pub const fn control_key(self) -> bool {
        self.contains(Self::CTRL)
    }
    #[must_use]
    pub const fn alt_key(self) -> bool {
        self.contains(Self::ALT)
    }
    #[must_use]
    pub const fn shift_key(self) -> bool {
        self.contains(Self::SHIFT)
    }
    #[must_use]
    pub const fn super_key(self) -> bool {
        self.contains(Self::SUPER)
    }

    /// [`InputMods`] bit assignments differ from this set's, and it has
    /// no `SUPER`: the mouse arm speaks xterm's shift / alt / ctrl
    /// triple, so a Super-modified drag reports unmodified.
    #[must_use]
    pub const fn to_input_mods(self) -> InputMods {
        let mut out = InputMods::empty();
        if self.control_key() {
            out = out.union(InputMods::CTRL);
        }
        if self.shift_key() {
            out = out.union(InputMods::SHIFT);
        }
        if self.alt_key() {
            out = out.union(InputMods::ALT);
        }
        out
    }
}

/// Function-key index, `1..=35` by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FKey(u8);

impl FKey {
    #[must_use]
    pub const fn new(n: u8) -> Option<Self> {
        if 1 <= n && n <= 35 {
            Some(Self(n))
        } else {
            None
        }
    }

    /// Panics outside `1..=35`; for constant arguments.
    #[must_use]
    pub const fn lit(n: u8) -> Self {
        match Self::new(n) {
            Some(k) => k,
            None => panic!("F-key index out of 1..=35"),
        }
    }

    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl fmt::Display for FKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The subset of a platform's named keys felis bridges; growing it also
/// needs the chord parser in `felis-client-core`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum NamedKey {
    Enter,
    Tab,
    Escape,
    Space,
    Backspace,
    Insert,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    F(FKey),
}

/// Only [`Numpad`](Self::Numpad) changes encoding (DECKPAM).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum KeyLocation {
    #[default]
    Standard,
    Left,
    Right,
    Numpad,
}

/// The logical key a physical keystroke resolved to, platform-free.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Key {
    Named(NamedKey),
    /// The key's own character, as the OS composed it; a dead-key chain
    /// may compose more than one grapheme. Capped at
    /// [`MAX_KEY_CHARACTER_BYTES`](crate::limits::MAX_KEY_CHARACTER_BYTES).
    Character(String),
    /// No terminal encoding of its own (dead key, bare modifier); the
    /// daemon defers to the [`KeyEvent::text`] passthrough.
    Other,
}

/// Press, auto-repeat, or release. Repeat is the Kitty keyboard
/// protocol's event type 2; every other encoding treats it as a press.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum KeyEventKind {
    #[default]
    Press,
    Repeat,
    Release,
}

/// One physical key event, carrying the platform-independent facts the
/// daemon needs to encode it against its own terminal modes
/// (`docs/explanation/input.md` "Keyboard").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyEvent {
    pub key: Key,
    /// The text the OS composed for this keystroke, when there is any.
    /// Capped at [`MAX_KEY_TEXT_BYTES`](crate::limits::MAX_KEY_TEXT_BYTES);
    /// a longer payload is a paste, not a keystroke.
    pub text: Option<String>,
    pub mods: KeyMods,
    pub kind: KeyEventKind,
    pub location: KeyLocation,
}

/// Mouse button reported by the client. Wheel events report as button +
/// [`MouseAction::Press`], as xterm encodes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    /// xterm button 64.
    WheelUp,
    /// xterm button 65.
    WheelDown,
    /// xterm button 66.
    WheelLeft,
    /// xterm button 67.
    WheelRight,
    /// xterm "high button" 8 (button-code 128). A device button outside
    /// 8..=11 has no xterm encoding, so the client drops it.
    Button8,
    /// xterm "high button" 9 (button-code 129).
    Button9,
    /// xterm "high button" 10 (button-code 130).
    Button10,
    /// xterm "high button" 11 (button-code 131).
    Button11,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseAction {
    Press,
    Release,
    /// Pointer moved while a button was held.
    Drag,
    /// Pointer moved with no button held (`?1003` any-motion
    /// reporting).
    Motion,
}

/// A single mouse event. Cell coordinates are 1-based to match xterm's
/// wire encoding; the pixel coordinates ride alongside for the `?1016`
/// SGR-pixel encoding, since only the client knows the cell metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseEvent {
    /// `None` when [`Self::action`] is [`MouseAction::Motion`].
    pub button: Option<MouseButton>,
    pub action: MouseAction,
    pub mods: InputMods,
    /// Cell column, 1-based.
    pub x: u16,
    /// Cell row, 1-based.
    pub y: u16,
    /// Pixel column within the terminal text area, 1-based.
    pub px: u16,
    /// Pixel row within the terminal text area, 1-based.
    pub py: u16,
}

/// Input-family messages ([`crate::MessageKind::Input`], kind 1).
/// Always client → daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputMsg {
    /// Already-encoded keystroke bytes, forwarded into the PTY
    /// verbatim: `sessions send --raw`, a committed IME string, and the
    /// `send_string` action. A physical keystroke rides [`Self::Key`].
    KeyBytes(Vec<u8>),
    /// Paste payload. The daemon brackets it (`\x1b[200~ … \x1b[201~`)
    /// only when the session has `?2004` set.
    Paste(Vec<u8>),
    /// The daemon encodes it per the active mouse-reporting protocol
    /// and encoding; events arriving while reporting is off are dropped
    /// silently.
    Mouse(MouseEvent),
    /// Window resize.
    Resize {
        /// The window's full geometry, pixel dims included. At wire
        /// width and not yet admitted: the daemon clamps it into the
        /// REQ-605a bounds, so a resize is never refused.
        dims: RequestedDims,
    },
    /// Window-focus change, forwarded as `CSI I` / `CSI O` when the
    /// session has `?1004` set. Each window reports its own OS focus;
    /// the daemon writes the PTY bytes only on the session-level edge
    /// (focused iff any window subscriber is focused).
    FocusChange { focused: bool },
    /// OS color-scheme report, sent on attach and theme change events.
    ///
    /// Answers `DSR ? 996 n` and drives `DECSET 2031` reports.
    /// Daemon evaluates preferences per window ownership hierarchy.
    ColorScheme {
        /// `true` = dark, `false` = light.
        dark: bool,
    },
    /// Scrollback viewport request. The daemon clamps to the current
    /// depth and replies with
    /// [`GridMsg::ViewportState`](super::GridMsg::ViewportState)
    /// carrying the effective value; while the viewport is non-zero it
    /// withholds row diffs and flushes a fresh frame on snap-back.
    Viewport {
        /// `0` is live; `N` shows the youngest `min(N, rows)` rows of
        /// scrollback at the top of the screen.
        lines_from_bottom: u32,
    },
    /// Move the scrollback viewport to the previous / next `OSC 133`
    /// prompt-start mark (`docs/explanation/data-model/scrollback.md`);
    /// the reply rides the normal viewport path. No-op when there is no
    /// prompt in that direction or the alternate screen is active.
    JumpPrompt { direction: PromptJump },
    /// Demand-driven frame request, sent once per vsync so the display
    /// refresh rate paces the grid stream
    /// (`docs/explanation/rendering/pipeline.md` "Demand-driven
    /// emission").
    NextGridFrame,
    /// A physical key event the daemon encodes itself, against the
    /// terminal modes it owns. The client resolves the logical key and
    /// its keymap bindings; only the mode-dependent byte encoding is
    /// the daemon's (`docs/explanation/input.md` "Keyboard").
    Key(KeyEvent),
}

const fn arm(name: &'static str) -> ArmMeta {
    ArmMeta::new(
        name,
        Direction::ToDaemon,
        CorrelationClass::Uncorrelated,
        ModeSet::ATTACHERS,
    )
}

impl Directed for InputMsg {
    const ARMS: &'static [ArmMeta] = &[
        arm("Input::KeyBytes"),
        arm("Input::Paste"),
        arm("Input::Mouse"),
        arm("Input::Resize"),
        arm("Input::FocusChange"),
        arm("Input::ColorScheme"),
        arm("Input::Viewport"),
        arm("Input::JumpPrompt"),
        arm("Input::NextGridFrame"),
        arm("Input::Key"),
    ];

    fn arm_index(&self) -> usize {
        match self {
            Self::KeyBytes(_) => 0,
            Self::Paste(_) => 1,
            Self::Mouse(_) => 2,
            Self::Resize { .. } => 3,
            Self::FocusChange { .. } => 4,
            Self::ColorScheme { .. } => 5,
            Self::Viewport { .. } => 6,
            Self::JumpPrompt { .. } => 7,
            Self::NextGridFrame => 8,
            Self::Key(_) => 9,
        }
    }
}

/// Direction for an [`InputMsg::JumpPrompt`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PromptJump {
    /// The nearest prompt above the current viewport top.
    Previous,
    /// The nearest prompt below the current viewport top.
    Next,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::messages::test_support::{assert_covers_every_arm, roundtrip};

    fn input_cases() -> Vec<InputMsg> {
        vec![
            InputMsg::KeyBytes(b"\x1b[A".to_vec()),
            InputMsg::Paste(b"hello world".to_vec()),
            InputMsg::Mouse(MouseEvent {
                button: Some(MouseButton::Left),
                action: MouseAction::Press,
                mods: InputMods::empty(),
                x: 1,
                y: 1,
                px: 3,
                py: 7,
            }),
            InputMsg::Mouse(MouseEvent {
                button: Some(MouseButton::WheelUp),
                action: MouseAction::Press,
                mods: InputMods::CTRL,
                x: 40,
                y: 12,
                px: 632,
                py: 288,
            }),
            InputMsg::Mouse(MouseEvent {
                button: None,
                action: MouseAction::Motion,
                mods: InputMods::SHIFT | InputMods::ALT,
                x: 80,
                y: 24,
                px: 1279,
                py: 575,
            }),
            InputMsg::Mouse(MouseEvent {
                button: Some(MouseButton::Button11),
                action: MouseAction::Release,
                mods: InputMods::all(),
                x: u16::MAX,
                y: u16::MAX,
                px: u16::MAX,
                py: u16::MAX,
            }),
            InputMsg::Mouse(MouseEvent {
                button: Some(MouseButton::Left),
                action: MouseAction::Drag,
                mods: InputMods::SHIFT,
                x: 10,
                y: 20,
                px: 100,
                py: 200,
            }),
            InputMsg::Resize {
                dims: RequestedDims {
                    rows: 24,
                    cols: 80,
                    pixel_w: 800,
                    pixel_h: 600,
                },
            },
            // A resize decodes at `uint32` width; the daemon clamps.
            InputMsg::Resize {
                dims: RequestedDims {
                    rows: u32::MAX,
                    cols: u32::MAX,
                    pixel_w: u32::MAX,
                    pixel_h: u32::MAX,
                },
            },
            InputMsg::FocusChange { focused: true },
            InputMsg::FocusChange { focused: false },
            InputMsg::ColorScheme { dark: true },
            InputMsg::ColorScheme { dark: false },
            InputMsg::Viewport {
                lines_from_bottom: 0,
            },
            InputMsg::Viewport {
                lines_from_bottom: 12_345,
            },
            InputMsg::Viewport {
                lines_from_bottom: u32::MAX,
            },
            InputMsg::JumpPrompt {
                direction: PromptJump::Previous,
            },
            InputMsg::JumpPrompt {
                direction: PromptJump::Next,
            },
            InputMsg::NextGridFrame,
            InputMsg::Key(KeyEvent {
                key: Key::Named(NamedKey::Enter),
                text: None,
                mods: KeyMods::empty(),
                kind: KeyEventKind::Press,
                location: KeyLocation::Standard,
            }),
            InputMsg::Key(KeyEvent {
                key: Key::Named(NamedKey::F(FKey::lit(35))),
                text: None,
                mods: KeyMods::all(),
                kind: KeyEventKind::Release,
                location: KeyLocation::Right,
            }),
            InputMsg::Key(KeyEvent {
                key: Key::Character("\u{e9}".into()),
                text: Some("\u{e9}".into()),
                mods: KeyMods::ALT | KeyMods::SUPER,
                kind: KeyEventKind::Repeat,
                location: KeyLocation::Numpad,
            }),
            // An empty text is not an absent one: the round trip must
            // keep them apart, so the wire field is `optional`.
            InputMsg::Key(KeyEvent {
                key: Key::Other,
                text: Some(String::new()),
                mods: KeyMods::CTRL,
                kind: KeyEventKind::Press,
                location: KeyLocation::Left,
            }),
        ]
    }

    #[test]
    fn key_mods_remap_every_bit_and_drop_super() {
        assert_eq!(KeyMods::CTRL.to_input_mods(), InputMods::CTRL);
        assert_eq!(KeyMods::SHIFT.to_input_mods(), InputMods::SHIFT);
        assert_eq!(KeyMods::ALT.to_input_mods(), InputMods::ALT);
        assert_eq!(KeyMods::SUPER.to_input_mods(), InputMods::empty());
        assert_eq!(KeyMods::empty().to_input_mods(), InputMods::empty());
        assert_eq!(KeyMods::all().to_input_mods(), InputMods::all());
    }

    #[test]
    fn input_messages_round_trip() {
        for msg in input_cases() {
            assert_eq!(roundtrip(&msg), msg);
        }
    }

    #[test]
    fn input_cases_cover_every_variant() {
        assert_covers_every_arm(&input_cases());
    }
}

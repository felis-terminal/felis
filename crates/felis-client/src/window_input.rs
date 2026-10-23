//! The window input `App` handles, in felis terms. `winit_keys` and the
//! `window_event` adapter translate winit's into it, so a test can drive
//! `App::on_window_input` without a window.

use std::path::PathBuf;

use felis_client_core::{Modifiers, NamedKey};
use felis_protocol::messages::{Key, KeyEvent, KeyEventKind, KeyLocation, MouseButton};
use winit::dpi::PhysicalPosition;

pub(crate) enum WindowInput {
    Modifiers(Modifiers),
    Focused(bool),
    ColorScheme {
        dark: bool,
    },
    PointerLeft,
    /// Physical window pixels.
    PointerMoved(PhysicalPosition<f64>),
    Button {
        pressed: bool,
        button: MouseButton,
    },
    Wheel(WheelDelta),
    Key(KeyPress),
    Ime(ImeInput),
    DroppedFile(PathBuf),
}

/// Positive `y` is away from the user (scroll up).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum WheelDelta {
    /// Notches, from a wheel.
    Lines(f32, f32),
    /// Physical pixels, from a touchpad.
    Pixels(f64, f64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImeInput {
    Enabled,
    /// The composition so far and its byte-offset cursor range.
    Preedit(String, Option<(usize, usize)>),
    Commit(String),
    Disabled,
}

/// One key event after Option composition (`option_compose`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeyPress {
    pub(crate) key: PressedKey,
    pub(crate) text: Option<String>,
    pub(crate) mods: Modifiers,
    pub(crate) kind: KeyEventKind,
    pub(crate) location: KeyLocation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PressedKey {
    Named(NamedKey),
    Character(String),
    /// Shift, Control, Alt, Alt Gr, Super, Meta or Caps Lock alone, which
    /// an open confirmation bar lets through on the way to `Y`.
    Modifier,
    /// Dead keys, media keys and every named key felis does not bind.
    Other,
}

impl KeyPress {
    pub(crate) fn is_press(&self) -> bool {
        self.kind != KeyEventKind::Release
    }

    /// The bytes it reaches the child as are the daemon's to decide,
    /// against the keyboard modes it owns (docs/explanation/input.md
    /// "Keyboard").
    pub(crate) fn to_wire(&self) -> KeyEvent {
        KeyEvent {
            key: match &self.key {
                PressedKey::Named(named) => Key::Named(*named),
                PressedKey::Character(s) => Key::Character(s.clone()),
                PressedKey::Modifier | PressedKey::Other => Key::Other,
            },
            text: self.text.clone(),
            mods: self.mods,
            kind: self.kind,
            location: self.location,
        }
    }
}

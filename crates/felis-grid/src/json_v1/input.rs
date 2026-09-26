//! v1 DTO for [`InputMsg`], the client→daemon terminal input a replay
//! has to be able to write back.

use felis_protocol::messages::{
    FKey, InputMods, InputMsg, Key, KeyEvent, KeyEventKind, KeyLocation, KeyMods, MouseAction,
    MouseButton, MouseEvent, NamedKey, PromptJump,
};

use super::JsonError;
use super::common::RequestedDimsJson;
use super::grid::plain_enum;

/// Lowest function-key index `FKey` admits.
pub const F_KEY_MIN: u8 = 1;

/// Highest function-key index `FKey` admits.
pub const F_KEY_MAX: u8 = 35;

/// Every key modifier this build defines.
pub const KEY_MODS_MAX: u8 = KeyMods::all().bits();

/// Every mouse modifier the xterm encoding has a bit for.
pub const INPUT_MODS_MAX: u8 = InputMods::all().bits();

json_dto! {
    #[serde(rename_all = "snake_case")]
    pub enum PromptJumpJson {
        Previous,
        Next,
    }

    #[serde(rename_all = "snake_case")]
    pub enum MouseButtonJson {
        Left,
        Middle,
        Right,
        WheelUp,
        WheelDown,
        WheelLeft,
        WheelRight,
        Button8,
        Button9,
        Button10,
        Button11,
    }

    #[serde(rename_all = "snake_case")]
    pub enum MouseActionJson {
        Press,
        Release,
        Drag,
        Motion,
    }

    #[serde(rename_all = "snake_case")]
    pub enum KeyEventKindJson {
        Press,
        Repeat,
        Release,
    }

    #[serde(rename_all = "snake_case")]
    pub enum KeyLocationJson {
        Standard,
        Left,
        Right,
        Numpad,
    }

    /// The named keys felis bridges.
    #[serde(tag = "name", rename_all = "snake_case")]
    pub enum NamedKeyJson {
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
        /// Function-key index, `1..=35`.
        F {
            #[cfg_attr(feature = "schema", schemars(range(min = F_KEY_MIN, max = F_KEY_MAX)))]
            index: u8,
        },
    }

    /// The logical key a keystroke resolved to.
    #[serde(tag = "key", rename_all = "snake_case")]
    pub enum KeyJson {
        Named { named: NamedKeyJson },
        Character { text: String },
        /// A dead key or a bare modifier: no encoding of its own.
        Other,
    }

    pub struct KeyEventJson {
        pub key: KeyJson,
        pub text: Option<String>,
        /// Bits 0–3: ctrl, shift, alt, super
        /// (`docs/reference/ipc.md` "Input (kind = 1)").
        #[cfg_attr(feature = "schema", schemars(range(max = KEY_MODS_MAX)))]
        pub mods: u8,
        pub kind: KeyEventKindJson,
        pub location: KeyLocationJson,
    }

    pub struct MouseEventJson {
        /// `null` for a bare motion.
        pub button: Option<MouseButtonJson>,
        pub action: MouseActionJson,
        /// Bits 0–2: shift, alt, ctrl. xterm has no Super bit.
        #[cfg_attr(feature = "schema", schemars(range(max = INPUT_MODS_MAX)))]
        pub mods: u8,
        /// Cell column, 1-based.
        pub x: u16,
        /// Cell row, 1-based.
        pub y: u16,
        /// Pixel column in the text area, 1-based.
        pub px: u16,
        /// Pixel row in the text area, 1-based.
        pub py: u16,
    }

    /// Input-family frames in their v1 form.
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum InputJson {
        KeyBytes { bytes: Vec<u8> },
        Paste { bytes: Vec<u8> },
        Mouse { event: MouseEventJson },
        Resize { dims: RequestedDimsJson },
        FocusChange { focused: bool },
        ColorScheme { dark: bool },
        Viewport { lines_from_bottom: u32 },
        JumpPrompt { direction: PromptJumpJson },
        NextGridFrame,
        Key { event: KeyEventJson },
    }
}

plain_enum!(PromptJumpJson, PromptJump, Previous, Next);
plain_enum!(
    MouseButtonJson,
    MouseButton,
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
    Button8,
    Button9,
    Button10,
    Button11
);
plain_enum!(MouseActionJson, MouseAction, Press, Release, Drag, Motion);
plain_enum!(KeyEventKindJson, KeyEventKind, Press, Repeat, Release);
plain_enum!(KeyLocationJson, KeyLocation, Standard, Left, Right, Numpad);

macro_rules! named_key {
    ($($variant:ident),+ $(,)?) => {
        impl From<NamedKey> for NamedKeyJson {
            fn from(key: NamedKey) -> Self {
                match key {
                    $(NamedKey::$variant => Self::$variant,)+
                    NamedKey::F(index) => Self::F { index: index.get() },
                }
            }
        }

        impl TryFrom<NamedKeyJson> for NamedKey {
            type Error = JsonError;

            fn try_from(key: NamedKeyJson) -> Result<Self, Self::Error> {
                Ok(match key {
                    $(NamedKeyJson::$variant => Self::$variant,)+
                    NamedKeyJson::F { index } => Self::F(
                        FKey::new(index)
                            .ok_or_else(|| JsonError::field("index", "a function key is 1..=35"))?,
                    ),
                })
            }
        }
    };
}

named_key!(
    Enter, Tab, Escape, Space, Backspace, Insert, Delete, Home, End, PageUp, PageDown, ArrowUp,
    ArrowDown, ArrowLeft, ArrowRight
);

impl From<Key> for KeyJson {
    fn from(key: Key) -> Self {
        match key {
            Key::Named(named) => Self::Named {
                named: named.into(),
            },
            Key::Character(text) => Self::Character { text },
            Key::Other => Self::Other,
        }
    }
}

impl TryFrom<KeyJson> for Key {
    type Error = JsonError;

    fn try_from(key: KeyJson) -> Result<Self, Self::Error> {
        Ok(match key {
            KeyJson::Named { named } => Self::Named(named.try_into()?),
            KeyJson::Character { text } => Self::Character(text),
            KeyJson::Other => Self::Other,
        })
    }
}

impl From<KeyEvent> for KeyEventJson {
    fn from(event: KeyEvent) -> Self {
        Self {
            key: event.key.into(),
            text: event.text,
            mods: event.mods.bits(),
            kind: event.kind.into(),
            location: event.location.into(),
        }
    }
}

impl TryFrom<KeyEventJson> for KeyEvent {
    type Error = JsonError;

    fn try_from(event: KeyEventJson) -> Result<Self, Self::Error> {
        Ok(Self {
            key: event.key.try_into()?,
            text: event.text,
            mods: KeyMods::from_bits(event.mods)
                .ok_or_else(|| JsonError::field("mods", "a reserved key modifier bit is set"))?,
            kind: event.kind.into(),
            location: event.location.into(),
        })
    }
}

impl From<MouseEvent> for MouseEventJson {
    fn from(event: MouseEvent) -> Self {
        Self {
            button: event.button.map(Into::into),
            action: event.action.into(),
            mods: event.mods.bits(),
            x: event.x,
            y: event.y,
            px: event.px,
            py: event.py,
        }
    }
}

impl TryFrom<MouseEventJson> for MouseEvent {
    type Error = JsonError;

    fn try_from(event: MouseEventJson) -> Result<Self, JsonError> {
        Ok(Self {
            button: event.button.map(Into::into),
            action: event.action.into(),
            mods: InputMods::from_bits(event.mods)
                .ok_or_else(|| JsonError::field("mods", "a reserved mouse modifier bit is set"))?,
            x: event.x,
            y: event.y,
            px: event.px,
            py: event.py,
        })
    }
}

impl From<InputMsg> for InputJson {
    fn from(msg: InputMsg) -> Self {
        match msg {
            InputMsg::KeyBytes(bytes) => Self::KeyBytes { bytes },
            InputMsg::Paste(bytes) => Self::Paste { bytes },
            InputMsg::Mouse(event) => Self::Mouse {
                event: event.into(),
            },
            InputMsg::Resize { dims } => Self::Resize { dims: dims.into() },
            InputMsg::FocusChange { focused } => Self::FocusChange { focused },
            InputMsg::ColorScheme { dark } => Self::ColorScheme { dark },
            InputMsg::Viewport { lines_from_bottom } => Self::Viewport { lines_from_bottom },
            InputMsg::JumpPrompt { direction } => Self::JumpPrompt {
                direction: direction.into(),
            },
            InputMsg::NextGridFrame => Self::NextGridFrame,
            InputMsg::Key(event) => Self::Key {
                event: event.into(),
            },
        }
    }
}

impl TryFrom<InputJson> for InputMsg {
    type Error = JsonError;

    fn try_from(msg: InputJson) -> Result<Self, Self::Error> {
        Ok(match msg {
            InputJson::KeyBytes { bytes } => Self::KeyBytes(bytes),
            InputJson::Paste { bytes } => Self::Paste(bytes),
            InputJson::Mouse { event } => Self::Mouse(event.try_into()?),
            InputJson::Resize { dims } => Self::Resize { dims: dims.into() },
            InputJson::FocusChange { focused } => Self::FocusChange { focused },
            InputJson::ColorScheme { dark } => Self::ColorScheme { dark },
            InputJson::Viewport { lines_from_bottom } => Self::Viewport { lines_from_bottom },
            InputJson::JumpPrompt { direction } => Self::JumpPrompt {
                direction: direction.into(),
            },
            InputJson::NextGridFrame => Self::NextGridFrame,
            InputJson::Key { event } => Self::Key(event.try_into()?),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_event(mods: u8) -> KeyEventJson {
        KeyEventJson {
            key: KeyJson::Other,
            text: None,
            mods,
            kind: KeyEventKindJson::Press,
            location: KeyLocationJson::Standard,
        }
    }

    fn mouse_event(mods: u8) -> MouseEventJson {
        MouseEventJson {
            button: None,
            action: MouseActionJson::Motion,
            mods,
            x: 1,
            y: 1,
            px: 1,
            py: 1,
        }
    }

    /// The published bounds are the constructor's, not a second pair
    /// written for the schema.
    #[test]
    fn the_published_function_key_bounds_are_the_ones_fkey_admits() {
        assert!(FKey::new(F_KEY_MIN).is_some());
        assert!(FKey::new(F_KEY_MAX).is_some());
        assert!(FKey::new(F_KEY_MIN - 1).is_none());
        assert!(FKey::new(F_KEY_MAX + 1).is_none());
    }

    /// A modifier this build does not define must not decode as the
    /// modifiers with that bit dropped, which would replay a different
    /// keystroke.
    #[test]
    fn a_reserved_key_modifier_bit_is_refused() {
        assert!(matches!(
            KeyEvent::try_from(key_event(KEY_MODS_MAX + 1)),
            Err(JsonError::Field { field: "mods", .. })
        ));
    }

    #[test]
    fn every_defined_key_modifier_bit_is_accepted() {
        assert!(KeyEvent::try_from(key_event(KEY_MODS_MAX)).is_ok());
    }

    #[test]
    fn a_reserved_mouse_modifier_bit_is_refused() {
        assert!(matches!(
            MouseEvent::try_from(mouse_event(INPUT_MODS_MAX + 1)),
            Err(JsonError::Field { field: "mods", .. })
        ));
    }

    #[test]
    fn every_defined_mouse_modifier_bit_is_accepted() {
        assert!(MouseEvent::try_from(mouse_event(INPUT_MODS_MAX)).is_ok());
    }
}

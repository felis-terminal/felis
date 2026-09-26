//! `InputMsg` <-> wire, with the mouse and key vocabulary it carries.

use super::{WireError, decode_enum, narrow};
use crate::messages::{self, InputMods, KeyMods};
use crate::wire::v1;

data_free_enum!("MouseAction", mouse_action_to_i32, mouse_action_from_i32, messages::MouseAction, v1::MouseAction, {
    Press => Press,
    Release => Release,
    Drag => Drag,
    Motion => Motion,
});

data_free_enum!("KeyLocation", key_location_to_i32, key_location_from_i32, messages::KeyLocation, v1::KeyLocation, {
    Standard => Standard,
    Left => Left,
    Right => Right,
    Numpad => Numpad,
});

data_free_enum!("KeyEventKind", key_event_kind_to_i32, key_event_kind_from_i32, messages::KeyEventKind, v1::KeyEventKind, {
    Press => Press,
    Repeat => Repeat,
    Release => Release,
});

data_free_enum!("PromptJump", prompt_jump_to_i32, prompt_jump_from_i32, messages::PromptJump, v1::PromptJump, {
    Previous => Previous,
    Next => Next,
});

impl From<messages::MouseButton> for v1::MouseButton {
    fn from(b: messages::MouseButton) -> Self {
        use messages::MouseButton as M;
        use v1::mouse_button::Button;
        let button = match b {
            M::Left => Button::Left(v1::MouseLeft {}),
            M::Middle => Button::Middle(v1::MouseMiddle {}),
            M::Right => Button::Right(v1::MouseRight {}),
            M::WheelUp => Button::WheelUp(v1::MouseWheelUp {}),
            M::WheelDown => Button::WheelDown(v1::MouseWheelDown {}),
            M::WheelLeft => Button::WheelLeft(v1::MouseWheelLeft {}),
            M::WheelRight => Button::WheelRight(v1::MouseWheelRight {}),
            M::Button8 => Button::Button8(v1::MouseButton8 {}),
            M::Button9 => Button::Button9(v1::MouseButton9 {}),
            M::Button10 => Button::Button10(v1::MouseButton10 {}),
            M::Button11 => Button::Button11(v1::MouseButton11 {}),
        };
        Self {
            button: Some(button),
        }
    }
}

impl TryFrom<v1::MouseButton> for messages::MouseButton {
    type Error = WireError;
    fn try_from(b: v1::MouseButton) -> Result<Self, Self::Error> {
        use v1::mouse_button::Button;
        Ok(
            match b
                .button
                .ok_or(WireError::MissingOneof("MouseButton.button"))?
            {
                Button::Left(_) => Self::Left,
                Button::Middle(_) => Self::Middle,
                Button::Right(_) => Self::Right,
                Button::WheelUp(_) => Self::WheelUp,
                Button::WheelDown(_) => Self::WheelDown,
                Button::WheelLeft(_) => Self::WheelLeft,
                Button::WheelRight(_) => Self::WheelRight,
                Button::Button8(_) => Self::Button8,
                Button::Button9(_) => Self::Button9,
                Button::Button10(_) => Self::Button10,
                Button::Button11(_) => Self::Button11,
            },
        )
    }
}

impl From<messages::MouseEvent> for v1::InputMouseEvent {
    fn from(e: messages::MouseEvent) -> Self {
        Self {
            button: e.button.map(Into::into),
            action: mouse_action_to_i32(e.action),
            mods: u32::from(e.mods.bits()),
            x: u32::from(e.x),
            y: u32::from(e.y),
            px: u32::from(e.px),
            py: u32::from(e.py),
        }
    }
}

impl TryFrom<v1::InputMouseEvent> for messages::MouseEvent {
    type Error = WireError;
    fn try_from(e: v1::InputMouseEvent) -> Result<Self, Self::Error> {
        Ok(Self {
            button: e.button.map(TryInto::try_into).transpose()?,
            action: mouse_action_from_i32(e.action)?,
            // Mask rather than reject: an undefined modifier bit degrades
            // to "not pressed" instead of costing the whole event.
            mods: InputMods::from_bits_truncate(e.mods.to_le_bytes()[0]),
            x: narrow("MouseEvent.x", e.x)?,
            y: narrow("MouseEvent.y", e.y)?,
            px: narrow("MouseEvent.px", e.px)?,
            py: narrow("MouseEvent.py", e.py)?,
        })
    }
}

const fn named_key_to_wire(named: messages::NamedKey) -> v1::key_id::Key {
    use messages::NamedKey as N;
    let wire = match named {
        N::Enter => v1::NamedKey::Enter,
        N::Tab => v1::NamedKey::Tab,
        N::Escape => v1::NamedKey::Escape,
        N::Space => v1::NamedKey::Space,
        N::Backspace => v1::NamedKey::Backspace,
        N::Insert => v1::NamedKey::Insert,
        N::Delete => v1::NamedKey::Delete,
        N::Home => v1::NamedKey::Home,
        N::End => v1::NamedKey::End,
        N::PageUp => v1::NamedKey::PageUp,
        N::PageDown => v1::NamedKey::PageDown,
        N::ArrowUp => v1::NamedKey::ArrowUp,
        N::ArrowDown => v1::NamedKey::ArrowDown,
        N::ArrowLeft => v1::NamedKey::ArrowLeft,
        N::ArrowRight => v1::NamedKey::ArrowRight,
        N::F(f) => return v1::key_id::Key::Function(f.get() as u32),
    };
    v1::key_id::Key::Named(wire as i32)
}

const fn named_key_from_wire(wire: v1::NamedKey) -> Result<messages::NamedKey, WireError> {
    use messages::NamedKey as N;
    Ok(match wire {
        v1::NamedKey::Unspecified => return Err(WireError::UnspecifiedEnum("NamedKey")),
        v1::NamedKey::Enter => N::Enter,
        v1::NamedKey::Tab => N::Tab,
        v1::NamedKey::Escape => N::Escape,
        v1::NamedKey::Space => N::Space,
        v1::NamedKey::Backspace => N::Backspace,
        v1::NamedKey::Insert => N::Insert,
        v1::NamedKey::Delete => N::Delete,
        v1::NamedKey::Home => N::Home,
        v1::NamedKey::End => N::End,
        v1::NamedKey::PageUp => N::PageUp,
        v1::NamedKey::PageDown => N::PageDown,
        v1::NamedKey::ArrowUp => N::ArrowUp,
        v1::NamedKey::ArrowDown => N::ArrowDown,
        v1::NamedKey::ArrowLeft => N::ArrowLeft,
        v1::NamedKey::ArrowRight => N::ArrowRight,
    })
}

impl From<&messages::Key> for v1::KeyId {
    fn from(k: &messages::Key) -> Self {
        use messages::Key as K;
        use v1::key_id::Key;
        let key = match k {
            K::Named(named) => named_key_to_wire(*named),
            K::Character(s) => Key::Character(s.clone()),
            K::Other => Key::Other(v1::KeyOther {}),
        };
        Self { key: Some(key) }
    }
}

impl TryFrom<v1::KeyId> for messages::Key {
    type Error = WireError;
    fn try_from(k: v1::KeyId) -> Result<Self, Self::Error> {
        use v1::key_id::Key;
        Ok(match k.key.ok_or(WireError::MissingOneof("KeyId.key"))? {
            Key::Named(raw) => Self::Named(named_key_from_wire(decode_enum("KeyId.named", raw)?)?),
            Key::Function(n) => {
                let index: u8 = narrow("KeyId.function", n)?;
                let f = messages::FKey::new(index)
                    .ok_or(WireError::MalformedField("KeyId.function"))?;
                Self::Named(messages::NamedKey::F(f))
            }
            Key::Character(s) => Self::Character(s),
            Key::Other(_) => Self::Other,
        })
    }
}

impl From<&messages::KeyEvent> for v1::InputKeyEvent {
    fn from(e: &messages::KeyEvent) -> Self {
        Self {
            key: Some((&e.key).into()),
            text: e.text.clone(),
            mods: u32::from(e.mods.bits()),
            kind: key_event_kind_to_i32(e.kind),
            location: key_location_to_i32(e.location),
        }
    }
}

impl TryFrom<v1::InputKeyEvent> for messages::KeyEvent {
    type Error = WireError;
    fn try_from(e: v1::InputKeyEvent) -> Result<Self, Self::Error> {
        Ok(Self {
            key: e
                .key
                .ok_or(WireError::MissingField("InputKeyEvent.key"))?
                .try_into()?,
            text: e.text,
            mods: KeyMods::from_bits_truncate(e.mods.to_le_bytes()[0]),
            kind: key_event_kind_from_i32(e.kind)?,
            location: key_location_from_i32(e.location)?,
        })
    }
}

impl From<&messages::InputMsg> for v1::InputMsg {
    fn from(m: &messages::InputMsg) -> Self {
        use messages::InputMsg as I;
        use v1::input_msg::Msg;
        let msg = match m {
            I::KeyBytes(b) => Msg::KeyBytes(b.clone()),
            I::Paste(b) => Msg::Paste(b.clone()),
            I::Mouse(e) => Msg::Mouse((*e).into()),
            I::Resize { dims } => Msg::Resize(v1::InputResize {
                dims: Some((*dims).into()),
            }),
            I::FocusChange { focused } => {
                Msg::FocusChange(v1::InputFocusChange { focused: *focused })
            }
            I::ColorScheme { dark } => Msg::ColorScheme(v1::InputColorScheme { dark: *dark }),
            I::Viewport { lines_from_bottom } => Msg::Viewport(v1::InputViewport {
                lines_from_bottom: *lines_from_bottom,
            }),
            I::JumpPrompt { direction } => Msg::JumpPrompt(v1::InputJumpPrompt {
                direction: prompt_jump_to_i32(*direction),
            }),
            I::NextGridFrame => Msg::NextGridFrame(v1::InputNextGridFrame {}),
            I::Key(event) => Msg::Key(event.into()),
        };
        Self { msg: Some(msg) }
    }
}

impl TryFrom<v1::InputMsg> for messages::InputMsg {
    type Error = WireError;
    fn try_from(m: v1::InputMsg) -> Result<Self, Self::Error> {
        use v1::input_msg::Msg;
        Ok(
            match m.msg.ok_or(WireError::MissingOneof("InputMsg.msg"))? {
                Msg::KeyBytes(b) => Self::KeyBytes(b),
                Msg::Paste(b) => Self::Paste(b),
                Msg::Mouse(e) => Self::Mouse(e.try_into()?),
                Msg::Resize(r) => Self::Resize {
                    dims: super::requested_dims_from_wire(r.dims, "InputResize.dims")?,
                },
                Msg::FocusChange(f) => Self::FocusChange { focused: f.focused },
                Msg::ColorScheme(c) => Self::ColorScheme { dark: c.dark },
                Msg::Viewport(v) => Self::Viewport {
                    lines_from_bottom: v.lines_from_bottom,
                },
                Msg::JumpPrompt(j) => Self::JumpPrompt {
                    direction: prompt_jump_from_i32(j.direction)?,
                },
                Msg::NextGridFrame(_) => Self::NextGridFrame,
                Msg::Key(event) => Self::Key(event.try_into()?),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::Key;

    fn key_id(key: v1::key_id::Key) -> v1::KeyId {
        v1::KeyId { key: Some(key) }
    }

    #[test]
    fn an_f_key_index_outside_the_supported_range_is_rejected() {
        for raw in [0, 36, u32::from(u16::MAX)] {
            assert!(matches!(
                Key::try_from(key_id(v1::key_id::Key::Function(raw))),
                Err(WireError::MalformedField("KeyId.function") | WireError::OutOfRange { .. })
            ));
        }
        assert_eq!(
            Key::try_from(key_id(v1::key_id::Key::Function(35))).unwrap(),
            Key::Named(messages::NamedKey::F(messages::FKey::lit(35))),
        );
    }

    #[test]
    fn an_absent_or_unspecified_named_key_is_rejected() {
        assert!(matches!(
            Key::try_from(v1::KeyId { key: None }),
            Err(WireError::MissingOneof("KeyId.key"))
        ));
        assert!(matches!(
            Key::try_from(key_id(v1::key_id::Key::Named(
                v1::NamedKey::Unspecified as i32
            ))),
            Err(WireError::UnspecifiedEnum("NamedKey"))
        ));
        assert!(matches!(
            Key::try_from(key_id(v1::key_id::Key::Named(99))),
            Err(WireError::UnknownEnum {
                field: "KeyId.named",
                value: 99
            })
        ));
    }

    #[test]
    fn a_key_event_without_a_key_is_rejected() {
        let event = v1::InputKeyEvent {
            key: None,
            text: None,
            mods: 0,
            kind: v1::KeyEventKind::Press as i32,
            location: v1::KeyLocation::Standard as i32,
        };
        assert!(matches!(
            messages::KeyEvent::try_from(event),
            Err(WireError::MissingField("InputKeyEvent.key"))
        ));
    }
}

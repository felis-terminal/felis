//! Keymap data model: platform-free types binding chords to
//! [`Action`](crate::action::Action) values (docs/reference/keybindings.md).
//! The winit -> `Chord` adapter lives in `felis-client` to keep this crate
//! platform-free.

mod chord;
mod default;
mod map;

pub use chord::{Chord, FKey, KeyCode, Modifiers, NamedKey, ParseChordError};
pub use map::{BindingValue, Keymap, PipeSink};

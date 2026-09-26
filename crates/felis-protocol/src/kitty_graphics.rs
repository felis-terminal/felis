//! Kitty graphics vocabulary shared between daemon and renderer: image and
//! placement identifiers and the Unicode-[`placeholder`] codec.

use serde::{Deserialize, Serialize};

pub mod placeholder;

/// Caller-assigned image id (Kitty `i=<u32>`). Image *numbers* (`I=`)
/// are resolved to ids upstream before reaching any store or wire
/// message. A bare `u32` on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ImageId(pub u32);

impl core::fmt::Display for ImageId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Caller-assigned placement id within an image (`p=<u32>`). `None` =
/// "the image's default placement", one implicit slot per image.
/// A bare `u32` on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PlacementId(pub u32);

impl core::fmt::Display for PlacementId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

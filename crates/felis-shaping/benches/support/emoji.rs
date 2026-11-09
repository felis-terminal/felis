//! The emoji faces and glyphs the Criterion `shape_cache` color benches
//! and their instruction-count twins rasterize, so the gated numbers and
//! the local wall-clock numbers measure the same code.

use std::{path::Path, sync::Arc};

use felis_shaping::{Font, FontStack, StyleFaces};

/// Faces, smileys, hands, animals, food and symbols: strikes of every
/// shape the face carries.
pub(crate) const CBDT: &[char] = &[
    '😀', '😂', '🥹', '😍', '🤔', '😴', '😭', '😱', '👍', '👋', '🙏', '💪', '🐱', '🐶', '🦀', '🐍',
    '🌈', '🔥', '⭐', '🌙', '🍣', '🍕', '☕', '🍺', '🎉', '🚀', '🚗', '⚽', '💡', '📦', '🔑', '❤',
    '✅', '❌', '⚠', '♻',
];

/// Every emoji the `COLR` probe keeps.
pub(crate) const COLR: &[char] = &[
    '😀', '❤', '🦀', '👍', '🎉', '✅', '⭐', '🔥', '🐱', '🌈', '🍣', '🚀',
];

pub(crate) const PX: u32 = 16;

/// The face `family` names in `file` under the pinned test-font
/// directory; panics outside the dev shell, which sets it.
fn face(file: &str, family: &str) -> Arc<Font> {
    let dir = std::env::var_os("FELIS_TEST_FONT_DIR")
        .expect("FELIS_TEST_FONT_DIR is unset: run the bench in the dev shell, which sets it");
    let path = Path::new(&dir).join(file);
    let stack =
        FontStack::discover_in_files(&[path], Some(family), &[], &[], &StyleFaces::default())
            .expect("the pinned emoji face loads");
    stack.primary().clone()
}

pub(crate) fn cbdt() -> Arc<Font> {
    face("NotoColorEmoji.ttf", "Noto Color Emoji")
}

pub(crate) fn colr() -> Arc<Font> {
    face("../color-probe/FelisColrProbe.ttf", "OpenMoji")
}

//! Faces the unit tests load by path.

use std::path::Path;

use crate::{Font, provider::files::FileSet, test_font_dir};

/// The first face of the font file at `path`.
fn load_path(path: &Path) -> Font {
    let mut set = FileSet::default();
    set.add_file(path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    Font::load_face_id(&set, 0).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// A face of the pinned set, named relative to `$FELIS_TEST_FONT_DIR`.
pub(crate) fn pinned(name: &str) -> Font {
    load_path(&test_font_dir().join(name))
}

/// One of the color-glyph probes built beside the pinned set.
pub(crate) fn color_probe(name: &str) -> Font {
    load_path(&test_font_dir().join("../color-probe").join(name))
}

/// One of the `COLR` version 1 faces pinned beside the set.
pub(crate) fn colrv1(name: &str) -> Font {
    load_path(&test_font_dir().join("../colrv1").join(name))
}

/// The glyph `font` names `name` in its `post` table.
pub(crate) fn glyph_named(font: &Font, name: &str) -> crate::GlyphId {
    let raw = skrifa::FontRef::from_index(font.bytes.as_ref().as_ref(), font.index)
        .expect("a loaded face parses");
    skrifa::GlyphNames::new(&raw)
        .iter()
        .find(|(_, n)| n.as_str() == name)
        .map_or_else(
            || panic!("no glyph named {name}"),
            |(glyph, _)| glyph.to_u32(),
        )
}

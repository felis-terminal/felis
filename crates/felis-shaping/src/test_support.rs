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

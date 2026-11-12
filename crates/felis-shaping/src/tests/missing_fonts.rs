use super::*;

#[test]
fn default_monospace_is_no_font_only_when_no_face_is_installed() {
    let result =
        FontStack::auto_discover_in(&FileSet::default(), None, &[], &[], &StyleFaces::default());
    assert!(matches!(result, Err(ShapingError::NoFont)));
}

#[test]
fn discover_in_files_reports_a_file_that_does_not_load() {
    let missing = std::path::PathBuf::from("/nonexistent/felis-font.ttf");
    let err = FontStack::discover_in_files(
        std::slice::from_ref(&missing),
        None,
        &[],
        &[],
        &StyleFaces::default(),
    )
    .err()
    .expect("a missing file cannot load");
    assert!(matches!(err, ShapingError::FontPath { path, .. } if path == missing));
}

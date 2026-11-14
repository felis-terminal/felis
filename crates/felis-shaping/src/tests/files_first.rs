use super::*;
use crate::provider::files_first::{FilesFirst, Layered};

const ARGON: &str = "opentype/MonaspaceArgon-Regular.otf";

const CJK: &str = "NotoSansCJKjp-Kuzu.otf";

/// The one code point the pinned CJK subset maps.
const KUZU: char = '\u{845B}';

fn files_first<S>(names: &[&str], system: S) -> FilesFirst<S> {
    FilesFirst {
        files: read_file_set(&pinned_files(names)).expect("the pinned files read"),
        system,
    }
}

fn argon_first() -> FontStack {
    let db = files_first(&[ARGON], pinned_system());
    FontStack::auto_discover_in(
        &db,
        Some("Monaspace Argon"),
        &[],
        &[],
        &StyleFaces::default(),
    )
    .expect("the file family loads")
}

#[test]
fn a_family_the_files_carry_resolves_there_at_every_style() {
    let db = files_first(&[ARGON], pinned_system());
    for (weight, italic) in [(REGULAR_WEIGHT, false), (BOLD_WEIGHT, true)] {
        assert!(matches!(
            db.find_face("Monaspace Argon", weight, italic),
            Some(Layered::File(_))
        ));
    }
    assert!(matches!(
        db.find_face("Monaspace Xenon", BOLD_WEIGHT, false),
        Some(Layered::System(_))
    ));
}

#[test]
fn the_files_fixed_pitch_face_is_the_default_ahead_of_the_systems() {
    let with_text_face = files_first(&[ARGON], pinned_system());
    assert!(matches!(
        with_text_face.default_monospace(REGULAR_WEIGHT),
        Ok((Layered::File(_), _))
    ));
    let mut named_but_proportional = files_first(&[CJK], pinned_system());
    named_but_proportional
        .files
        .set_monospace_family(PINNED_CJK_FAMILY);
    assert!(matches!(
        named_but_proportional.default_monospace(REGULAR_WEIGHT),
        Ok((Layered::System(_), _))
    ));
}

#[test]
fn a_code_point_the_files_lack_draws_from_the_systems_fonts() {
    let stack = argon_first();
    let index = stack.resolve_styled_index(KUZU, FontStyle::REGULAR);
    assert_ne!(index, 0);
    assert!(stack.font_at(index, FontStyle::REGULAR).has_glyph(KUZU));
    let files_only = FontStack::discover_in_files(
        &pinned_files(&[ARGON]),
        Some("Monaspace Argon"),
        &[],
        &[],
        &StyleFaces::default(),
    )
    .expect("the file family loads");
    assert_eq!(files_only.resolve_styled_index(KUZU, FontStyle::REGULAR), 0);
}

/// The system holds every Argon style; the files hold the regular alone.
#[test]
fn styles_of_a_file_family_never_mix_in_its_system_namesake() {
    let db = files_first(&[ARGON], pinned_system());
    let xenon = FaceSpec::named("Monaspace Xenon");
    let styles = StyleFaces {
        italic: &xenon,
        ..StyleFaces::default()
    };
    let stack = FontStack::auto_discover_in(&db, Some("Monaspace Argon"), &[], &[], &styles)
        .expect("the file family loads");
    let primary = stack.describe().primary;
    let bold = FontStyle {
        bold: true,
        italic: false,
    };
    assert_eq!(primary[bold.index()], primary[FontStyle::REGULAR.index()]);
    let italic = FontStyle {
        bold: false,
        italic: true,
    };
    assert!(
        primary[italic.index()].starts_with("Monaspace Xenon"),
        "{}",
        primary[italic.index()]
    );
}

#[test]
fn an_unreadable_file_fails_discovery_before_the_system_is_asked() {
    let missing = std::path::PathBuf::from("/nonexistent/felis-font.ttf");
    let err = FontStack::discover_files_first(
        std::slice::from_ref(&missing),
        None,
        &[],
        &[],
        &StyleFaces::default(),
    )
    .err()
    .expect("a missing file is rejected");
    assert!(matches!(err, ShapingError::FontPath { path, .. } if path == missing));
}

/// fontconfig knows no file family, and the pinned rules name the test
/// family for `monospace`.
#[cfg(target_os = "linux")]
#[test]
fn a_file_family_takes_the_cascade_the_host_ranks_for_monospace() {
    let db = files_first(&[ARGON], pinned_system());
    let cascade = db
        .fallback_cascade("Monaspace Argon", &HashMap::new())
        .expect("fontconfig ranks a cascade");
    let first = cascade.first().expect("the cascade has a face").id;
    assert!(matches!(first, Layered::System(_)));
    assert_eq!(db.family_of(first).as_deref(), Some(TEST_FONT_FAMILY));
}

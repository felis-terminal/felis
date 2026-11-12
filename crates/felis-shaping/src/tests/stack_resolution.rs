use super::*;

/// A typo in `font.family` falls back to `Family::Monospace` and
/// picks the same face an unset `font.family` would.
#[test]
fn unknown_primary_family_falls_back_to_monospace() {
    let fallback = discover(
        Some("definitely-not-a-real-family-xyz123"),
        &[],
        &[],
        &StyleFaces::default(),
    )
    .expect("fallback to monospace");
    let baseline = stack();
    // Metrics stand in for face identity; the `Arc`s are distinct loads.
    assert_eq!(
        fallback.primary().cell_metrics(16.0).width,
        baseline.primary().cell_metrics(16.0).width
    );
}

/// A monospace default the set does not hold, as Debian's fontconfig
/// rules name `FreeMono` whether or not it is installed.
#[test]
fn uninstalled_monospace_generic_falls_back_to_an_installed_fixed_pitch_family() {
    let dir = test_font_dir();
    let mut db = FileSet::default();
    db.add_dir(dir);
    db.set_monospace_family("FreeMono");

    let (id, family) = db
        .default_monospace(REGULAR_WEIGHT)
        .expect("a fixed-pitch face is installed");
    let (style, monospace) = db.traits_of(id).expect("resolved id names a face");
    assert!(monospace);
    assert_eq!(db.family_of(id).as_deref(), Some(family.as_str()));
    assert_eq!(
        style,
        FaceStyle {
            italic: false,
            weight: REGULAR_WEIGHT
        }
    );

    let stack = FontStack::auto_discover_in(&db, None, &[], &[], &StyleFaces::default())
        .expect("default config loads");
    assert!(
        !Arc::ptr_eq(
            stack.styled_primary(FontStyle::REGULAR),
            stack.styled_primary(FontStyle {
                bold: true,
                italic: false,
            })
        ),
        "bold derives from the fallback family, not the unresolved generic",
    );
}

#[test]
fn has_glyph_true_for_ascii_in_monospace_font() {
    let font = font();
    for c in ['A', 'a', '0', '!', ' ', 'M'] {
        assert!(font.has_glyph(c), "monospace must cover ASCII {c:?}");
    }
}

#[test]
fn has_glyph_false_for_unmapped_private_use_codepoint() {
    let font = font();
    assert!(!font.has_glyph('\u{F8FFD}'));
}

#[test]
fn stack_with_only_primary_resolves_to_primary() {
    let primary = Arc::new(font());
    let stack = FontStack::new(primary.clone());
    assert!(Arc::ptr_eq(
        stack.resolve('A', FontStyle::REGULAR),
        &primary
    ));
    assert!(Arc::ptr_eq(
        stack.resolve('\u{F8FFD}', FontStyle::REGULAR),
        &primary
    ));
    assert_eq!(stack.len(), 1);
}

#[test]
fn stack_resolve_picks_first_match() {
    let primary = Arc::new(font());
    // The same face twice, so the test needs no second font in CI.
    let secondary = Arc::new(font());
    let stack = FontStack::new(primary.clone()).with_fallback_features(secondary, Vec::new());
    assert!(
        Arc::ptr_eq(stack.resolve('A', FontStyle::REGULAR), &primary),
        "primary must win when both have the glyph"
    );
    assert_eq!(stack.len(), 2);
}

#[test]
fn stack_resolve_walks_past_primary_when_it_lacks_glyph() {
    let dir = test_font_dir();
    let mut db = FileSet::default();
    db.add_dir(dir);
    let primary = Arc::new(Font::try_load_with(&db, TEST_FONT_FAMILY).expect("pinned primary"));
    let cjk = Arc::new(Font::try_load_with(&db, PINNED_CJK_FAMILY).expect("pinned CJK face"));
    let cjk_target = '\u{845B}';
    assert!(!primary.has_glyph(cjk_target));
    let stack = FontStack::new(primary).with_fallback_features(cjk.clone(), Vec::new());
    assert!(
        Arc::ptr_eq(stack.resolve(cjk_target, FontStyle::REGULAR), &cjk),
        "fallback must win when primary lacks the glyph"
    );
}

/// Text-presentation-default symbols (✳ ✻ …) resolve mono, not to
/// the color-emoji face. Runs the real discovery order against the
/// pinned devshell set (Monaspace + Noto Sans Symbols 2 + Noto Color
/// Emoji, which also covers them); the host's own fontconfig may lack
/// a mono home and legitimately go color.
#[test]
fn text_presentation_dingbats_resolve_to_mono_not_color() {
    let dir = test_font_dir();
    let mut db = FileSet::default();
    db.add_dir(dir);
    // The pinned db has no fontconfig generic-family aliases, so
    // `Family::Monospace` would not resolve.
    let stack = FontStack::auto_discover_in(
        &db,
        Some(TEST_FONT_FAMILY),
        &[],
        &[],
        &StyleFaces::default(),
    )
    .expect("Monaspace primary must load from the pinned font dir");
    let mut ctx = Rasterizer::default();
    for c in ['✳', '✻', '✶', '✢'] {
        let font = stack.resolve(c, FontStyle::REGULAR);
        assert!(
            font.has_glyph(c),
            "the pinned symbol face must cover U+{:04X}",
            c as u32,
        );
        let bitmap = rasterize(&mut ctx, font, c, 32.0);
        assert!(
            !bitmap.is_color(),
            "U+{:04X} must rasterize as a monochrome glyph, got color \
             (symbol fallback should win before the emoji font)",
            c as u32,
        );
    }
}

#[test]
fn auto_discover_returns_at_least_the_primary() {
    let stack = stack();
    assert!(!stack.is_empty());
    assert!(stack.primary().has_glyph('A'));
}

#[test]
fn a_missing_explicit_fallback_leaves_auto_discovery_as_it_was() {
    let missing = discover(
        None,
        &[],
        &[FaceSpec::named("definitely-not-a-real-family-xyz123")],
        &StyleFaces::default(),
    )
    .expect("primary must load");
    let auto = discover(None, &[], &[], &StyleFaces::default()).expect("primary must load");
    assert_eq!(missing.describe().fallbacks, auto.describe().fallbacks);
}

/// The pinned Neon statics name their wide cuts with the normal
/// family, so which file comes first must not pick the width.
#[test]
fn a_file_set_family_resolves_to_its_normal_width_in_any_file_order() {
    let truetype = test_font_dir().join("truetype");
    let normal = truetype.join("MonaspaceNeonFrozen-Regular.ttf");
    let wide = truetype.join("MonaspaceNeonFrozen-WideRegular.ttf");
    for order in [[&normal, &wide], [&wide, &normal]] {
        let mut files = FileSet::default();
        for path in order {
            files.add_file(path).expect("the pinned file loads");
        }
        let family = files.family_of(0).expect("the first file has a family");
        assert_eq!(files.family_of(1).as_ref(), Some(&family));
        let id = files
            .find_face(&family, REGULAR_WEIGHT, false)
            .expect("the family resolves");
        let expected = usize::from(order[0] == &wide);
        assert_eq!(id, expected, "{order:?}");
    }
}

/// A fallback the system may well have (`DejaVu Sans Mono`) is absent from the
/// pinned files, so it must not join the stack.
#[test]
fn discover_in_files_resolves_from_the_given_fonts_alone() {
    let files = test_font_files();
    let stack = FontStack::discover_in_files(
        &files,
        Some(TEST_FONT_FAMILY),
        &[],
        &[FaceSpec::named("DejaVu Sans Mono")],
        &StyleFaces::default(),
    )
    .expect("the pinned family loads");
    let pinned = Font::load_test_font();
    let dims = |m: CellMetrics| (m.width, m.height, m.ascent);
    assert_eq!(
        dims(stack.primary().cell_metrics(14.0)),
        dims(pinned.cell_metrics(14.0))
    );
    assert_eq!(stack.len(), 1);
}

#[test]
fn a_described_stack_names_each_primary_style_from_the_name_table() {
    let files = test_font_files();
    let stack = FontStack::discover_in_files(
        &files,
        Some(TEST_FONT_FAMILY),
        &[],
        &[],
        &StyleFaces::default(),
    )
    .expect("the pinned family loads");
    let described = stack.describe();
    for name in &described.primary {
        assert!(name.starts_with(TEST_FONT_FAMILY), "{name}");
    }
    assert_ne!(described.primary[0], described.primary[1]);
    assert_eq!(described.fallbacks, Vec::<String>::new());
    assert_eq!(described.system_fallback, None);
}

/// A readable file with no face in it fails like a missing one.
#[test]
fn discover_in_files_reports_a_file_with_no_face() {
    let mut files = test_font_files();
    let not_a_font = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"));
    files.push(not_a_font.clone());
    let err = FontStack::discover_in_files(
        &files,
        Some(TEST_FONT_FAMILY),
        &[],
        &[],
        &StyleFaces::default(),
    )
    .err()
    .expect("a file with no face is rejected even when the others load");
    assert!(matches!(err, ShapingError::FontPath { path, .. } if path == not_a_font));
}

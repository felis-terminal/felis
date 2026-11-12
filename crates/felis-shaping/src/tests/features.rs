use super::*;

#[test]
fn auto_discover_inherits_primary_features_into_fallbacks() {
    let features = vec!["calt".to_owned(), "-clig".to_owned()];
    let stack = discover(None, &features, &[], &StyleFaces::default()).expect("primary must load");
    assert_eq!(stack.features_at(0), features.as_slice());
    for i in 0..stack.len() {
        assert_eq!(
            stack.features_at(i),
            features.as_slice(),
            "fallback index {i} must inherit the primary's features",
        );
    }
}

/// A lone `font.fallback` entry naming `family`, over the pinned set.
fn pinned_explicit_fallback(
    primary_features: &[String],
    family: &str,
    features: Option<Vec<String>>,
) -> FontStack {
    let provider = pinned_system();
    let spec = FaceSpec {
        family: Some(family.to_owned()),
        features,
        ..FaceSpec::default()
    };
    let stack = FontStack::auto_discover_in(
        &provider,
        None,
        primary_features,
        &[spec],
        &StyleFaces::default(),
    )
    .expect("primary must load");
    assert!(
        stack.describe().fallbacks[0].starts_with(family),
        "the declared entry comes right after the primary"
    );
    stack
}

#[test]
fn explicit_fallback_with_empty_features_opts_out() {
    let primary_features = vec!["calt".to_owned(), "ss01".to_owned()];
    let stack = pinned_explicit_fallback(&primary_features, "Noto Color Emoji", Some(Vec::new()));
    assert_eq!(stack.features_at(0), primary_features.as_slice());
    let empty: &[String] = &[];
    assert_eq!(
        stack.features_at(1),
        empty,
        "explicit empty features is an opt-out, not an inherit"
    );
}

#[test]
fn explicit_fallback_without_features_inherits_primary() {
    let primary_features = vec!["calt".to_owned(), "ss01".to_owned()];
    let stack = pinned_explicit_fallback(&primary_features, "Noto Color Emoji", None);
    assert_eq!(
        stack.features_at(1),
        primary_features.as_slice(),
        "an omitted features list inherits, it does not opt out"
    );
}

#[test]
fn explicit_fallback_with_features_overrides_primary() {
    let override_features = vec!["palt".to_owned(), "trad".to_owned()];
    let stack = pinned_explicit_fallback(
        &["calt".to_owned()],
        PINNED_CJK_FAMILY,
        Some(override_features.clone()),
    );
    assert_eq!(stack.features_at(0), &["calt".to_owned()]);
    assert_eq!(stack.features_at(1), override_features.as_slice());
}

/// Feature lists follow the face `font_at` picks; a style with no
/// override inherits the base list.
#[test]
fn styled_feature_override_applies_where_the_styled_face_does() {
    let base = vec!["calt".to_owned()];
    let bold_features = vec!["-liga".to_owned()];
    let stack = discover(
        None,
        &base,
        &[],
        &StyleFaces {
            bold: &FaceSpec {
                features: Some(bold_features.clone()),
                ..FaceSpec::default()
            },
            ..StyleFaces::default()
        },
    )
    .expect("primary must load");

    let regular = FontStyle {
        bold: false,
        italic: false,
    };
    let bold = FontStyle {
        bold: true,
        italic: false,
    };
    let italic = FontStyle {
        bold: false,
        italic: true,
    };
    assert_eq!(stack.features_for(0, regular), base.as_slice());
    assert_eq!(stack.features_for(0, bold), bold_features.as_slice());
    assert_eq!(
        stack.features_for(0, italic),
        base.as_slice(),
        "a style with no override inherits the base features",
    );
}

/// A bare `"ss03"` must reach GSUB: Monaspace keeps its arrows in
/// `ss03`, off by default, so a glyph-id change proves the feature
/// arrived (a `calt` check passes vacuously). Also pins the two-glyph
/// blank-lead + body form `apply_shaped_run` in felis-render-core
/// depends on.
#[test]
fn shape_run_applies_non_default_stylistic_set() {
    let font = Font::load_test_font();
    let mut shaper = Shaper::new();
    let plain = shaper.shape_run(&font, 16.0, &[], "->");
    let ss03 = shaper.shape_run(&font, 16.0, &["ss03".to_owned()], "->");
    let plain_ids: Vec<GlyphId> = plain.iter().map(|g| g.glyph_id).collect();
    let ss03_ids: Vec<GlyphId> = ss03.iter().map(|g| g.glyph_id).collect();
    assert_ne!(
        plain_ids, ss03_ids,
        "ss03 must swap the `->` glyphs for the arrow alternates",
    );
    assert_eq!(ss03.len(), 2, "Monaspace arrows are blank-lead + body");
    assert!(
        ss03.iter().all(|g| g.source_byte_len == 2),
        "both arrow pieces must claim the full 2-byte cluster",
    );
}

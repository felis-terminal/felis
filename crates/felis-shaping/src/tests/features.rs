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

/// A case name, the primary's features, the fallback family, its
/// declared features and the features it ends up with.
type Case<'a> = (
    &'a str,
    &'a [String],
    &'a str,
    Option<Vec<String>>,
    &'a [String],
);

#[test]
fn explicit_fallback_features_opt_out_when_empty_inherit_when_omitted_and_override_when_set() {
    let calt_ss01 = ["calt", "ss01"].map(str::to_owned);
    let calt = ["calt".to_owned()];
    let palt_trad = ["palt", "trad"].map(str::to_owned);
    let cases: [Case<'_>; 3] = [
        (
            "empty",
            &calt_ss01,
            "Noto Color Emoji",
            Some(Vec::new()),
            &[],
        ),
        ("omitted", &calt_ss01, "Noto Color Emoji", None, &calt_ss01),
        (
            "set",
            &calt,
            PINNED_CJK_FAMILY,
            Some(palt_trad.to_vec()),
            &palt_trad,
        ),
    ];
    for (case, primary_features, family, features, expected) in cases {
        let stack = pinned_explicit_fallback(primary_features, family, features);
        assert_eq!(stack.features_at(0), primary_features, "{case}: primary");
        assert_eq!(stack.features_at(1), expected, "{case}: fallback");
    }
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

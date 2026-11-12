use super::*;

#[test]
fn italic_position_takes_both_italic_axes_from_one_instance() {
    let axes = [
        axis(*b"wght", 400.0),
        axis(*b"ital", 0.0),
        axis(*b"slnt", 0.0),
    ];
    let instances = [
        instance("Regular", &[400.0, 0.0, 0.0]),
        instance("Italic", &[400.0, 1.0, -8.0]),
    ];
    assert_eq!(
        italic_position(400.0, &axes, &instances),
        [(ITAL, 1.0), (SLNT, -8.0)]
    );
}

#[test]
fn italic_position_picks_the_instance_nearest_the_target_weight() {
    let axes = [axis(*b"wght", 400.0), axis(*b"slnt", 0.0)];
    let instances = [
        instance("Light Italic", &[300.0, -9.0]),
        instance("BoldItalic", &[700.0, -12.0]),
        instance("Italic", &[400.0, -10.0]),
    ];
    assert_eq!(italic_position(400.0, &axes, &instances), [(SLNT, -10.0)]);
    assert_eq!(italic_position(700.0, &axes, &instances), [(SLNT, -12.0)]);
}

#[test]
fn italic_position_breaks_a_weight_tie_by_fvar_order() {
    let axes = [axis(*b"wght", 400.0), axis(*b"slnt", 0.0)];
    let instances = [
        instance("Light Italic", &[300.0, -9.0]),
        instance("Medium Italic", &[500.0, -11.0]),
    ];
    assert_eq!(italic_position(400.0, &axes, &instances), [(SLNT, -9.0)]);
}

#[test]
fn italic_position_without_a_weight_axis_takes_the_first_italic_instance() {
    let axes = [axis(*b"slnt", 0.0)];
    let instances = [
        instance("Oblique", &[-10.0]),
        instance("Extra-Oblique", &[-15.0]),
    ];
    assert_eq!(italic_position(400.0, &axes, &instances), [(SLNT, -10.0)]);
}

#[test]
fn italic_position_ignores_italics_at_another_width() {
    let axes = [
        axis(*b"wght", 400.0),
        axis(*b"wdth", 100.0),
        axis(*b"slnt", 0.0),
    ];
    let instances = [
        instance("SemiWide Italic", &[400.0, 112.5, -14.0]),
        instance("Italic", &[400.0, 100.0, -11.0]),
    ];
    assert_eq!(italic_position(400.0, &axes, &instances), [(SLNT, -11.0)]);
    let wide_only = [instance("Wide Italic", &[400.0, 125.0, -11.0])];
    assert_eq!(italic_position(400.0, &axes, &wide_only), Vec::new());
}

#[test]
fn italic_position_falls_back_to_ital_one() {
    let axes = [
        axis(*b"wght", 400.0),
        axis(*b"ital", 0.0),
        axis(*b"slnt", 0.0),
    ];
    assert_eq!(
        italic_position(400.0, &axes, &[instance("Regular", &[400.0, 0.0, 0.0])]),
        [(ITAL, 1.0)]
    );
}

#[test]
fn italic_position_leaves_a_bare_slnt_axis_upright() {
    let axes = [axis(*b"wght", 400.0), axis(*b"slnt", 0.0)];
    let instances = [
        instance("Regular", &[400.0, 0.0]),
        instance("Italicized", &[400.0, -10.0]),
    ];
    assert_eq!(italic_position(400.0, &axes, &instances), Vec::new());
}

#[test]
fn italic_position_is_empty_without_an_italic_axis() {
    let axes = [axis(*b"wght", 400.0)];
    assert_eq!(
        italic_position(400.0, &axes, &[instance("Italic", &[400.0])]),
        Vec::new()
    );
}

#[test]
fn italic_token_splits_joined_and_separated_style_names() {
    for name in [
        "Italic",
        "Bold Italic",
        "BoldItalic",
        "Bold-Italic",
        "Light_Oblique",
        "ITALIC",
    ] {
        assert!(has_italic_token(name), "{name}");
    }
    for name in ["Regular", "Italicized", "Obliquity", "Bold"] {
        assert!(!has_italic_token(name), "{name}");
    }
}

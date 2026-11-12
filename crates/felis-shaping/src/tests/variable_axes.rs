use super::*;

#[test]
fn a_variation_view_is_named_with_the_position_it_draws_at() {
    let font = pinned_variable_font();
    let bold = font.at_style(700, false).expect("the face has a wght axis");
    let bold_italic = font.at_style(700, true).expect("the face has a slnt axis");
    let heaviest = font
        .at_style(1000, false)
        .expect("the face has a wght axis");
    assert!(
        !font.display_name().ends_with(')'),
        "{}",
        font.display_name()
    );
    assert!(
        bold.display_name().ends_with(" (wght=700)"),
        "{}",
        bold.display_name()
    );
    assert!(
        bold_italic
            .display_name()
            .ends_with(" (wght=700, slnt=-11)"),
        "{}",
        bold_italic.display_name()
    );
    assert!(
        heaviest.display_name().ends_with(" (wght=800)"),
        "{}",
        heaviest.display_name()
    );
}

/// The pinned variable face defaults to `ExtraLight`, so a stack that
/// draws its default instance renders regular and bold alike, too thin.
#[test]
fn variable_face_draws_regular_and_bold_at_their_weights() {
    let dir = test_font_dir();
    let file = std::path::Path::new(&dir).join("truetype/Monaspace Neon Var.ttf");
    let family = "Monaspace Neon Var";
    let stack = FontStack::discover_in_files(
        std::slice::from_ref(&file),
        Some(family),
        &[],
        &[],
        &StyleFaces::default(),
    )
    .expect("the variable face loads");
    let mut db = FileSet::default();
    db.add_file(&file).expect("the variable face reads");
    let default_instance = Font::try_load_with(&db, family).expect("the variable face loads");
    let ink = |font: &Font| {
        let bitmap = rasterize(&mut Rasterizer::default(), font, 'M', 32.0);
        bitmap
            .pixels()
            .as_bytes()
            .iter()
            .map(|&p| u64::from(p))
            .sum::<u64>()
    };
    let bold = FontStyle {
        bold: true,
        italic: false,
    };
    let default_ink = ink(&default_instance);
    let regular_ink = ink(stack.styled_primary(FontStyle::REGULAR));
    let bold_ink = ink(stack.styled_primary(bold));
    assert!(
        default_ink < regular_ink && regular_ink < bold_ink,
        "ink must grow ExtraLight < Regular < Bold: {default_ink} {regular_ink} {bold_ink}",
    );
}

/// The variable face defaults to `ExtraLight`, so a fallback loaded at
/// its default instance draws thinner than the primary beside it.
#[test]
fn variable_fallback_draws_at_the_regular_weight() {
    let mut files = test_font_files();
    let dir = test_font_dir();
    files.push(dir.join("truetype/Monaspace Neon Var.ttf"));
    let stack = FontStack::discover_in_files(
        &files,
        Some(TEST_FONT_FAMILY),
        &[],
        &[FaceSpec::named("Monaspace Neon Var")],
        &StyleFaces::default(),
    )
    .expect("the pinned faces load");
    let fallback = stack.font_at(1, FontStyle::REGULAR);
    let wght = fallback
        .face
        .axes()
        .iter()
        .position(|v| v.tag() == WGHT)
        .expect("the face has wght");
    assert_eq!(
        fallback.coords.get(wght),
        Some(&design_value(fallback, WGHT, 400.0)),
        "the fallback sits at wght 400",
    );
}

fn pinned_variable_stack(styles: &StyleFaces<'_>) -> (FontStack, std::path::PathBuf) {
    let dir = test_font_dir();
    let files = [
        dir.join("truetype/Monaspace Neon Var.ttf"),
        dir.join("opentype/MonaspaceNeon-Italic.otf"),
    ];
    let stack = FontStack::discover_in_files(&files, Some("Monaspace Neon Var"), &[], &[], styles)
        .expect("the pinned faces load");
    (stack, files[0].clone())
}

/// Monaspace Neon Var ships no italic file: its "Italic" and "Bold
/// Italic" named instances sit at `slnt` -11 on the upright file.
#[test]
fn variable_face_draws_italic_at_its_named_italic_slant() {
    let (stack, _) = pinned_variable_stack(&StyleFaces::default());
    let regular = stack.styled_primary(FontStyle::REGULAR);
    let italic = stack.styled_primary(FontStyle {
        bold: false,
        italic: true,
    });
    let bold_italic = stack.styled_primary(FontStyle {
        bold: true,
        italic: true,
    });
    let slanted = design_value(regular, SLNT, -11.0);
    let slnt_at = |font: &Font| {
        let index = font
            .face
            .axes()
            .iter()
            .position(|v| v.tag() == SLNT)
            .expect("the face has slnt");
        font.coords[index]
    };
    assert_eq!(slnt_at(regular), F2Dot14::ZERO);
    assert_eq!(slnt_at(italic), slanted);
    assert_eq!(slnt_at(bold_italic), slanted);
    assert_eq!(
        bold_italic.coords[0],
        design_value(regular, WGHT, 700.0),
        "bold italic keeps the bold weight",
    );
    let ink = |font: &Font| {
        rasterize(&mut Rasterizer::default(), font, 'M', 32.0)
            .pixels()
            .as_bytes()
            .to_vec()
    };
    assert_ne!(ink(regular), ink(italic));
}

#[test]
fn separate_italic_file_keeps_its_own_design() {
    let italic_family = FaceSpec::named("Monaspace Neon");
    let styles = StyleFaces {
        italic: &italic_family,
        ..StyleFaces::default()
    };
    let (stack, _) = pinned_variable_stack(&styles);
    let italic = stack.styled_primary(FontStyle {
        bold: false,
        italic: true,
    });
    assert!(
        italic.coords.is_empty(),
        "a static italic face takes no axis settings"
    );
    assert!(!Arc::ptr_eq(
        italic,
        stack.styled_primary(FontStyle::REGULAR)
    ));
}

/// The pinned CJK subset varies `wght` only, so bold italic lands on
/// the bold view and italic on the regular one.
#[test]
fn styles_at_the_same_position_share_one_face() {
    let dir = test_font_dir();
    let file = std::path::Path::new(&dir).join("NotoSansCJKjp-Kuzu.otf");
    let stack = FontStack::discover_in_files(
        std::slice::from_ref(&file),
        Some("Noto Sans CJK JP"),
        &[],
        &[],
        &StyleFaces::default(),
    )
    .expect("the pinned CJK subset loads");
    let face = |bold, italic| stack.styled_primary(FontStyle { bold, italic });
    assert!(
        !face(false, false).coords.is_empty(),
        "the face is variable"
    );
    assert!(Arc::ptr_eq(face(true, false), face(true, true)));
    assert!(Arc::ptr_eq(face(false, false), face(false, true)));
    assert!(!Arc::ptr_eq(face(false, false), face(true, false)));
}

const fn setting(tag: [u8; 4], value: f32) -> AxisSetting {
    AxisSetting { tag, value }
}

#[test]
fn a_regular_weight_moves_regular_and_italic_and_leaves_bold_at_700() {
    let styles = StyleFaces {
        regular_weight: Some(500),
        ..StyleFaces::default()
    };
    let (stack, _) = pinned_variable_stack(&styles);
    let regular = stack.styled_primary(FontStyle::REGULAR);
    let medium = design_value(regular, WGHT, 500.0);
    let bold = design_value(regular, WGHT, 700.0);
    assert_eq!(axis_coord(regular, *b"wght"), medium);
    assert_eq!(axis_coord(stack.styled_primary(ITALIC), *b"wght"), medium);
    assert_eq!(axis_coord(stack.styled_primary(BOLD), *b"wght"), bold);
    assert_eq!(
        axis_coord(stack.styled_primary(BOLD_ITALIC), *b"wght"),
        bold
    );
}

#[test]
fn bold_italic_takes_the_bold_weight_unless_it_sets_its_own() {
    let bold = FaceSpec {
        weight: Some(800),
        ..FaceSpec::default()
    };
    let styles = StyleFaces {
        bold: &bold,
        ..StyleFaces::default()
    };
    let (stack, _) = pinned_variable_stack(&styles);
    let regular = stack.styled_primary(FontStyle::REGULAR);
    let heavy = design_value(regular, WGHT, 800.0);
    assert_eq!(axis_coord(stack.styled_primary(BOLD), *b"wght"), heavy);
    assert_eq!(
        axis_coord(stack.styled_primary(BOLD_ITALIC), *b"wght"),
        heavy
    );

    let bold_italic = FaceSpec {
        weight: Some(600),
        ..FaceSpec::default()
    };
    let styles = StyleFaces {
        bold: &bold,
        bold_italic: &bold_italic,
        ..StyleFaces::default()
    };
    let (stack, _) = pinned_variable_stack(&styles);
    assert_eq!(
        axis_coord(stack.styled_primary(BOLD_ITALIC), *b"wght"),
        design_value(regular, WGHT, 600.0)
    );
}

/// `instances().nth(16)` of the pinned variable face is `SemiWide
/// Regular`.
#[test]
fn an_unset_regular_weight_keeps_a_named_instance_and_a_set_one_moves_only_wght() {
    let default = pinned_variable_font();
    let semiwide = || default.at_instance(16).expect("the instance exists");
    assert!(semiwide().regular_view(None, &[]).is_none());
    let light = semiwide().regular_view(Some(300), &[]).expect("wght moves");
    assert_eq!(
        axis_coord(&light, *b"wdth"),
        axis_coord(&semiwide(), *b"wdth")
    );
    assert_eq!(
        axis_coord(&light, *b"wght"),
        design_value(&light, WGHT, 300.0)
    );
}

/// The pinned variable face has no `HVAR`; its `wdth` axis varies the
/// advance through `gvar` phantom points alone.
#[test]
fn a_width_axis_without_hvar_widens_the_cell() {
    let default = pinned_variable_font();
    let wide = default
        .regular_view(None, &[setting(*b"wdth", 125.0)])
        .expect("wdth moves");
    let (narrow, wide) = (default.cell_metrics(20.0), wide.cell_metrics(20.0));
    assert!(
        wide.width > narrow.width,
        "{} must exceed {}",
        wide.width,
        narrow.width
    );
}

#[test]
fn static_styles_are_matched_at_their_configured_weights() {
    let files = pinned_files(&[
        "opentype/MonaspaceNeon-Light.otf",
        "opentype/MonaspaceNeon-LightItalic.otf",
        "opentype/MonaspaceNeon-Regular.otf",
        "opentype/MonaspaceNeon-Italic.otf",
        "opentype/MonaspaceNeon-Bold.otf",
        "opentype/MonaspaceNeon-ExtraBold.otf",
        "opentype/MonaspaceNeon-ExtraBoldItalic.otf",
    ]);
    let bold = FaceSpec {
        weight: Some(800),
        ..FaceSpec::default()
    };
    let styles = StyleFaces {
        regular_weight: Some(300),
        bold: &bold,
        ..StyleFaces::default()
    };
    let stack = FontStack::discover_in_files(&files, Some("Monaspace Neon"), &[], &[], &styles)
        .expect("the pinned faces load");
    let name = |style| stack.styled_primary(style).display_name();
    assert!(
        name(FontStyle::REGULAR).ends_with("Light"),
        "{}",
        name(FontStyle::REGULAR)
    );
    assert!(name(ITALIC).ends_with("Light Italic"), "{}", name(ITALIC));
    assert!(name(BOLD).ends_with("ExtraBold"), "{}", name(BOLD));
    assert!(
        name(BOLD_ITALIC).ends_with("ExtraBold Italic"),
        "{}",
        name(BOLD_ITALIC)
    );
}

#[test]
fn derived_styles_inherit_the_regular_axes_but_not_its_slant() {
    let regular_axes = [setting(*b"wdth", 110.0), setting(*b"slnt", -5.0)];
    let styles = StyleFaces {
        regular_axes: &regular_axes,
        ..StyleFaces::default()
    };
    let (stack, _) = pinned_variable_stack(&styles);
    let regular = stack.styled_primary(FontStyle::REGULAR);
    let wdth = |font: &Font| axis_coord(font, *b"wdth");
    let slnt = |font: &Font| axis_coord(font, *b"slnt");
    let width = design_value(regular, Tag::new(b"wdth"), 110.0);
    assert_eq!(wdth(regular), width);
    assert_eq!(slnt(regular), design_value(regular, SLNT, -5.0));
    for style in [BOLD, ITALIC, BOLD_ITALIC] {
        assert_eq!(wdth(stack.styled_primary(style)), width, "{style:?}");
    }
    assert_eq!(slnt(stack.styled_primary(BOLD)), F2Dot14::ZERO);
    assert_eq!(
        slnt(stack.styled_primary(ITALIC)),
        design_value(regular, SLNT, -11.0),
        "italic keeps its named slant"
    );
}

#[test]
fn a_style_naming_its_own_family_inherits_no_axes() {
    let regular_axes = [setting(*b"wdth", 110.0)];
    let bold = FaceSpec::named("Monaspace Neon Var");
    let styles = StyleFaces {
        regular_axes: &regular_axes,
        bold: &bold,
        ..StyleFaces::default()
    };
    let (stack, _) = pinned_variable_stack(&styles);
    assert_eq!(
        axis_coord(stack.styled_primary(BOLD), *b"wdth"),
        F2Dot14::ZERO
    );
    assert_ne!(
        axis_coord(stack.styled_primary(BOLD_ITALIC), *b"wdth"),
        F2Dot14::ZERO
    );
}

#[test]
fn an_explicit_slant_replaces_the_named_italic_position() {
    let italic = FaceSpec {
        axes: vec![setting(*b"slnt", -4.0)],
        ..FaceSpec::default()
    };
    let styles = StyleFaces {
        italic: &italic,
        ..StyleFaces::default()
    };
    let (stack, _) = pinned_variable_stack(&styles);
    let regular = stack.styled_primary(FontStyle::REGULAR);
    assert_eq!(
        axis_coord(stack.styled_primary(ITALIC), *b"slnt"),
        design_value(regular, SLNT, -4.0)
    );
    assert_eq!(
        axis_coord(stack.styled_primary(BOLD_ITALIC), *b"slnt"),
        design_value(regular, SLNT, -11.0)
    );
}

#[test]
fn a_wght_entry_in_axes_and_an_axis_the_face_lacks_are_ignored() {
    let regular_axes = [setting(*b"ZZZZ", 1.0), setting(*b"wght", 800.0)];
    let styles = StyleFaces {
        regular_axes: &regular_axes,
        ..StyleFaces::default()
    };
    let (moved, _) = pinned_variable_stack(&styles);
    let (unset, _) = pinned_variable_stack(&StyleFaces::default());
    for index in 0..4 {
        let style = FontStyle::from_index(index);
        assert_eq!(
            moved.styled_primary(style).coords,
            unset.styled_primary(style).coords,
            "{style:?}"
        );
    }
}

#[test]
fn an_axis_value_outside_the_face_range_is_clamped() {
    let regular_axes = [setting(*b"wdth", 500.0)];
    let styles = StyleFaces {
        regular_axes: &regular_axes,
        ..StyleFaces::default()
    };
    let (stack, _) = pinned_variable_stack(&styles);
    let regular = stack.styled_primary(FontStyle::REGULAR);
    let widest = regular
        .face
        .axes()
        .get_by_tag(Tag::new(b"wdth"))
        .expect("the face has wdth")
        .max_value();
    assert_eq!(
        axis_coord(regular, *b"wdth"),
        design_value(regular, Tag::new(b"wdth"), widest)
    );
}

#[test]
fn an_explicit_fallback_is_drawn_at_the_configured_weights() {
    let mut files = test_font_files();
    files.push(test_font_dir().join("truetype/Monaspace Neon Var.ttf"));
    let bold = FaceSpec {
        weight: Some(800),
        ..FaceSpec::default()
    };
    let styles = StyleFaces {
        regular_weight: Some(300),
        bold: &bold,
        ..StyleFaces::default()
    };
    let stack = FontStack::discover_in_files(
        &files,
        Some(TEST_FONT_FAMILY),
        &[],
        &[FaceSpec::named("Monaspace Neon Var")],
        &styles,
    )
    .expect("the pinned faces load");
    let fallback = |style| stack.font_at(1, style);
    let regular = fallback(FontStyle::REGULAR);
    assert_eq!(
        axis_coord(regular, *b"wght"),
        design_value(regular, WGHT, 300.0)
    );
    assert_eq!(
        axis_coord(fallback(BOLD), *b"wght"),
        design_value(regular, WGHT, 800.0)
    );
}

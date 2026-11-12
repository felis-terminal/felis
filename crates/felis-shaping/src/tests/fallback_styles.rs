use super::*;
use crate::test_support::{pinned_arc, pinned_dir_set};

/// One fallback a [`ScriptedProvider`] ranks.
struct ScriptedFace {
    /// `None` stands for a file no font parser reads.
    bytes: Option<FontBytes>,
    reported_color: bool,
    /// Pruned while this earlier face is kept in the same class, as a
    /// face adding no coverage is.
    shadowed_by: Option<usize>,
}

/// Ranks `fallbacks` (ids 1..) after `primary` (id 0) the way a
/// coverage-pruning provider would, honoring what loading showed.
struct ScriptedProvider {
    primary: FontBytes,
    fallbacks: Vec<ScriptedFace>,
}

impl FaceProvider for ScriptedProvider {
    type Id = usize;

    fn find_face(&self, _: &str, _: u16, _: bool) -> Option<usize> {
        Some(0)
    }

    fn default_monospace(&self, _: u16) -> Result<(usize, String), ShapingError> {
        Ok((0, "primary".to_owned()))
    }

    fn resolve_face(&self, id: usize) -> Option<ResolvedFace> {
        let bytes = match id {
            0 => self.primary.clone(),
            n => self.fallbacks[n - 1]
                .bytes
                .clone()
                .unwrap_or_else(|| Arc::new(vec![0_u8; 64])),
        };
        Some(ResolvedFace {
            source: FaceSource::Shared(bytes),
            index: 0,
            instance: None,
            slanted: false,
            hinting: None,
        })
    }

    fn fallback_cascade(
        &self,
        _: &str,
        loaded: &HashMap<usize, LoadedAs>,
    ) -> Option<Vec<provider::CascadeFace<usize>>> {
        let mut kept: Vec<provider::CascadeFace<usize>> = Vec::new();
        for (i, face) in self.fallbacks.iter().enumerate() {
            let id = i + 1;
            let color = match loaded.get(&id) {
                Some(LoadedAs::Unusable) => continue,
                Some(LoadedAs::Color(color)) => *color,
                None => face.reported_color,
            };
            let shadowed = face
                .shadowed_by
                .is_some_and(|by| kept.iter().any(|k| k.id == by && k.color == color));
            if !shadowed {
                kept.push(provider::CascadeFace { id, color });
            }
        }
        Some(kept)
    }
}

fn scripted_stack(fallbacks: Vec<ScriptedFace>) -> FontStack {
    FontStack::auto_discover_in(
        &ScriptedProvider {
            primary: pinned_bytes("opentype/MonaspaceNeon-Regular.otf"),
            fallbacks,
        },
        None,
        &[],
        &[],
        &StyleFaces::default(),
    )
    .expect("the primary loads")
}

fn pinned_bytes(name: &str) -> FontBytes {
    let files = pinned_files(&[name]);
    Arc::new(std::fs::read(&files[0]).expect("the pinned face reads"))
}

/// Finds every named family at a face no parser reads (id 1); the
/// monospace face (id 0) loads.
struct UnreadableNamedFaces {
    monospace: FontBytes,
}

impl FaceProvider for UnreadableNamedFaces {
    type Id = usize;

    fn find_face(&self, _: &str, _: u16, _: bool) -> Option<usize> {
        Some(1)
    }

    fn default_monospace(&self, _: u16) -> Result<(usize, String), ShapingError> {
        Ok((0, TEST_FONT_FAMILY.to_owned()))
    }

    fn resolve_face(&self, id: usize) -> Option<ResolvedFace> {
        let bytes = if id == 0 {
            self.monospace.clone()
        } else {
            Arc::new(vec![0_u8; 64])
        };
        Some(ResolvedFace {
            source: FaceSource::Shared(bytes),
            index: 0,
            instance: None,
            slanted: false,
            hinting: None,
        })
    }
}

#[test]
fn a_font_family_whose_face_fails_to_load_falls_back_to_monospace() {
    let monospace = pinned_bytes("opentype/MonaspaceNeon-Regular.otf");
    let stack = FontStack::auto_discover_in(
        &UnreadableNamedFaces { monospace },
        Some("Unreadable"),
        &[],
        &[],
        &StyleFaces::default(),
    )
    .expect("the monospace face loads");
    assert!(stack.primary().has_glyph('A'));
}

#[test]
fn a_fallback_that_fails_to_load_yields_its_coverage_to_the_next() {
    let symbols = pinned_bytes("NotoSansSymbols2-Regular.otf");
    let stack = scripted_stack(vec![
        ScriptedFace {
            bytes: None,
            reported_color: false,
            shadowed_by: None,
        },
        ScriptedFace {
            bytes: Some(symbols),
            reported_color: false,
            shadowed_by: Some(1),
        },
    ]);
    assert_eq!(stack.len(), 2);
    assert_ne!(
        stack.resolve_styled_index('\u{2702}', FontStyle::REGULAR),
        0
    );
}

/// The emoji face is reported mono, so it first shadows the mono
/// symbol face; once it loads as color, the symbol face stops being
/// shadowed and joins the chain.
#[test]
fn a_fallback_reported_mono_but_loaded_color_stops_shadowing_mono_faces() {
    let (emoji, symbols) = (
        pinned_bytes("NotoColorEmoji.ttf"),
        pinned_bytes("NotoSansSymbols2-Regular.otf"),
    );
    let stack = scripted_stack(vec![
        ScriptedFace {
            bytes: Some(emoji),
            reported_color: false,
            shadowed_by: None,
        },
        ScriptedFace {
            bytes: Some(symbols),
            reported_color: false,
            shadowed_by: Some(1),
        },
    ]);
    assert_eq!(stack.len(), 3);
    let text = stack.resolve_styled_index('\u{2702}', FontStyle::REGULAR);
    assert!(!stack.font_at(text, FontStyle::REGULAR).has_color_glyphs());
}

const MONASPACE_NEON_STATIC: [&str; 4] = [
    "opentype/MonaspaceNeon-Regular.otf",
    "opentype/MonaspaceNeon-Bold.otf",
    "opentype/MonaspaceNeon-Italic.otf",
    "opentype/MonaspaceNeon-BoldItalic.otf",
];

/// The pinned CJK subset varies `wght` only and has no italic.
#[test]
fn variable_fallback_draws_bold_cells_bold() {
    let mut names = MONASPACE_NEON_STATIC.to_vec();
    names.push("NotoSansCJKjp-Kuzu.otf");
    let files = pinned_files(&names);
    let stack = FontStack::discover_in_files(
        &files,
        Some(TEST_FONT_FAMILY),
        &[],
        &[FaceSpec::named("Noto Sans CJK JP")],
        &StyleFaces::default(),
    )
    .expect("the pinned faces load");
    assert_eq!(stack.resolve_styled_index('葛', BOLD), 1);
    let regular = stack.font_at(1, FontStyle::REGULAR);
    let bold = stack.font_at(1, BOLD);
    assert_eq!(bold.coords, [design_value(bold, WGHT, 700.0)]);
    assert!(ink(regular, '葛') < ink(bold, '葛'));
    assert!(
        Arc::ptr_eq(stack.font_at(1, ITALIC), regular),
        "no italic is synthesized"
    );
    assert!(Arc::ptr_eq(stack.font_at(1, BOLD_ITALIC), bold));
    assert!(
        Arc::ptr_eq(stack.font_at(2, BOLD), stack.styled_primary(BOLD)),
        "an index past the chain is the styled primary"
    );
}

#[test]
fn static_fallback_draws_each_style_from_its_own_file() {
    let mut names = MONASPACE_NEON_STATIC.to_vec();
    names.push("NotoSansCJKjp-Kuzu.otf");
    let files = pinned_files(&names);
    let stack = FontStack::discover_in_files(
        &files,
        Some("Noto Sans CJK JP"),
        &[],
        &[FaceSpec::named(TEST_FONT_FAMILY)],
        &StyleFaces::default(),
    )
    .expect("the pinned faces load");
    let faces = [FontStyle::REGULAR, BOLD, ITALIC, BOLD_ITALIC].map(|s| stack.font_at(1, s));
    for (i, a) in faces.iter().enumerate() {
        assert!(a.coords.is_empty(), "static faces take no axis settings");
        for b in &faces[i + 1..] {
            assert!(!Arc::ptr_eq(a, b), "every style has its own file");
        }
    }
    assert!(ink(faces[0], 'M') < ink(faces[1], 'M'));
}

#[test]
fn color_fallback_draws_the_same_face_in_every_style() {
    let files = pinned_files(&[MONASPACE_NEON_STATIC[0], "NotoColorEmoji.ttf"]);
    let stack = FontStack::discover_in_files(
        &files,
        Some(TEST_FONT_FAMILY),
        &[],
        &[FaceSpec::named("Noto Color Emoji")],
        &StyleFaces::default(),
    )
    .expect("the pinned faces load");
    let regular = stack.font_at(1, FontStyle::REGULAR);
    assert!(regular.has_color_glyphs());
    for style in [BOLD, ITALIC, BOLD_ITALIC] {
        assert!(Arc::ptr_eq(stack.font_at(1, style), regular));
    }
}

#[test]
fn coverage_check_rejects_a_face_missing_a_regular_codepoint() {
    let (latin, cjk) = (
        pinned_arc(MONASPACE_NEON_STATIC[0]),
        pinned_arc("NotoSansCJKjp-Kuzu.otf"),
    );
    assert!(!latin.covers_all_of(&cjk));
    assert!(!cjk.covers_all_of(&latin));
    let bold = cjk.at_style(700, false).expect("the subset varies wght");
    assert!(bold.covers_all_of(&cjk), "a moved view shares the cmap");
    assert!(Arc::ptr_eq(
        &covering_or_regular(latin, &cjk, "Noto Sans CJK JP", BOLD),
        &cjk
    ));
}

#[test]
fn coverage_check_accepts_a_sibling_file_with_the_same_cmap() {
    let (regular, bold) = (
        pinned_arc(MONASPACE_NEON_STATIC[0]),
        pinned_arc(MONASPACE_NEON_STATIC[1]),
    );
    assert!(bold.covers_all_of(&regular));
}

/// Entry 1's bold view covers `A` and its regular does not; a bold `A`
/// still goes to entry 2, as a regular one does.
#[test]
fn a_styled_view_covering_more_does_not_take_a_later_entrys_glyph() {
    let (emoji, latin, cjk) = (
        pinned_arc("NotoColorEmoji.ttf"),
        pinned_arc(MONASPACE_NEON_STATIC[0]),
        pinned_arc("NotoSansCJKjp-Kuzu.otf"),
    );
    assert!(!emoji.has_glyph('A') && !cjk.has_glyph('A'));
    let wider_bold = [cjk.clone(), latin.clone(), cjk, latin.clone()];
    let stack = FontStack::new(emoji)
        .with_fallback_views(wider_bold, Vec::new())
        .with_fallback_features(latin, Vec::new());
    assert_eq!(stack.resolve_styled_index('A', FontStyle::REGULAR), 2);
    assert_eq!(stack.resolve_styled_index('A', BOLD), 2);
}

/// `primary_covers` asks the styled primary, not `fonts[0]`; the
/// renderer breaks a shape run when the styled face stops covering.
/// Bold is pinned to Noto Sans Symbols 2 (⌨, no Latin) against
/// Monaspace Neon (the reverse).
#[test]
fn primary_covers_answers_from_the_styled_primary_not_the_regular_one() {
    let db = pinned_dir_set();
    let base = TEST_FONT_FAMILY;
    let regular_id = db
        .find_face(base, REGULAR_WEIGHT, false)
        .expect("regular Monaspace Neon must resolve");
    let regular = Arc::new(Font::load_face_id(&db, regular_id).expect("load regular face"));
    let bold = FontStyle {
        bold: true,
        italic: false,
    };
    let symbols = FaceSpec::named("Noto Sans Symbols 2");
    let mut stack = FontStack::with_primary_features(regular.clone(), Vec::new());
    stack.load_styled(
        &db,
        regular_id,
        &regular,
        &regular,
        base,
        &[],
        &StyleFaces {
            bold: &symbols,
            ..StyleFaces::default()
        },
    );
    assert!(
        !Arc::ptr_eq(stack.styled_primary(bold), stack.primary()),
        "the bold override must have loaded a face of its own",
    );

    assert!(stack.primary_covers('A', FontStyle::REGULAR));
    assert!(
        !stack.primary_covers('A', bold),
        "a bold 'A' the bold face lacks must report uncovered",
    );
    assert!(stack.primary_covers('\u{2328}', bold));
    assert!(!stack.primary_covers('\u{2328}', FontStyle::REGULAR));
}

/// Styled primaries load distinct faces when the family ships them,
/// and a missing override dedups to the regular `Arc`.
#[test]
fn styled_primaries_pick_real_faces_and_dedup_missing_to_regular() {
    let db = pinned_dir_set();
    let base = TEST_FONT_FAMILY;
    let regular_id = db
        .find_face(base, REGULAR_WEIGHT, false)
        .expect("regular Monaspace Neon must resolve");
    let regular = Arc::new(Font::load_face_id(&db, regular_id).expect("load regular face"));

    let bold = FontStyle {
        bold: true,
        italic: false,
    };
    let italic = FontStyle {
        bold: false,
        italic: true,
    };
    let bold_italic = FontStyle {
        bold: true,
        italic: true,
    };

    let mut stack = FontStack::with_primary_features(regular.clone(), Vec::new());
    stack.load_styled(
        &db,
        regular_id,
        &regular,
        &regular,
        base,
        &[],
        &StyleFaces::default(),
    );
    assert!(
        !Arc::ptr_eq(
            stack.styled_primary(FontStyle::REGULAR),
            stack.styled_primary(bold)
        ),
        "Monaspace Neon ships a real Bold face distinct from Regular",
    );
    assert!(
        !Arc::ptr_eq(
            stack.styled_primary(FontStyle::REGULAR),
            stack.styled_primary(italic)
        ),
        "…and a real Italic",
    );
    assert!(
        !Arc::ptr_eq(
            stack.styled_primary(bold),
            stack.styled_primary(bold_italic)
        ),
        "Bold and BoldItalic are distinct faces",
    );
    assert!(Arc::ptr_eq(
        stack.resolve('A', bold),
        stack.styled_primary(bold)
    ));

    let absent = FaceSpec::named("definitely-not-a-real-family-xyz123");
    let missing = StyleFaces {
        bold: &absent,
        ..StyleFaces::default()
    };
    let mut stack2 = FontStack::with_primary_features(regular.clone(), Vec::new());
    stack2.load_styled(&db, regular_id, &regular, &regular, base, &[], &missing);
    assert!(
        Arc::ptr_eq(stack2.styled_primary(bold), &regular),
        "a missing bold override falls back to the regular face",
    );
}

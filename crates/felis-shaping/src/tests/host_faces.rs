use super::*;

const UMBRELLA: char = '\u{2602}';

const SCISSORS: char = '\u{2702}';

const HOT_BEVERAGE: char = '\u{2615}';

/// Answers each query with the pinned file `answers` names for it,
/// counting the questions; `fail_first` fails the first one.
struct ScriptedSystem {
    answers: HashMap<String, &'static str>,
    asked: Arc<AtomicUsize>,
    fail_first: AtomicBool,
}

impl SystemFallback for ScriptedSystem {
    fn name(&self) -> &'static str {
        "scripted"
    }

    fn face_for(&self, query: &str, _: FontStyle) -> Result<Option<ResolvedFace>, ShapingError> {
        self.asked.fetch_add(1, Ordering::Relaxed);
        if self.fail_first.swap(false, Ordering::Relaxed) {
            return Err(ShapingError::BadFont("transient".to_owned()));
        }
        Ok(self.answers.get(query).map(|name| ResolvedFace {
            source: FaceSource::File(test_font_dir().join(name)),
            index: 0,
            instance: None,
            slanted: false,
            hinting: None,
        }))
    }
}

fn scripted_system(answers: &[(&str, &'static str)]) -> (ScriptedSystem, Arc<AtomicUsize>) {
    let asked = Arc::new(AtomicUsize::new(0));
    let system = ScriptedSystem {
        answers: answers
            .iter()
            .map(|&(query, name)| (query.to_owned(), name))
            .collect(),
        asked: asked.clone(),
        fail_first: AtomicBool::new(false),
    };
    (system, asked)
}

fn system_stack(answers: &[(&str, &'static str)]) -> (FontStack, Arc<AtomicUsize>) {
    let (system, asked) = scripted_system(answers);
    let stack = FontStack::new(Arc::new(font())).with_system_fallback(Box::new(system));
    (stack, asked)
}

#[test]
fn a_scalar_no_ordered_face_covers_draws_from_the_system_face_asked_once() {
    let (stack, asked) = system_stack(&[("\u{2602}", SYMBOLS2)]);
    assert_eq!(
        stack.describe().system_fallback.as_deref(),
        Some("scripted")
    );
    let index = stack.resolve_styled_index(UMBRELLA, FontStyle::REGULAR);
    assert_eq!(index, stack.len());
    assert!(stack.font_at(index, FontStyle::REGULAR).has_glyph(UMBRELLA));
    assert_eq!(
        stack.resolve_styled_index(UMBRELLA, FontStyle::REGULAR),
        index
    );
    assert_eq!(asked.load(Ordering::Relaxed), 1);
}

/// The emoji face the host picks for the scissors also covers the
/// umbrella, which the host draws from the symbol face.
#[test]
fn a_system_face_answers_only_the_scalars_the_host_gave_it() {
    let umbrella_in_color = |order: [char; 2]| {
        let (stack, _) = system_stack(&[("\u{2702}", EMOJI), ("\u{2602}", SYMBOLS2)]);
        for c in order {
            assert_ne!(stack.resolve_styled_index(c, FontStyle::REGULAR), 0);
        }
        let index = stack.resolve_styled_index(UMBRELLA, FontStyle::REGULAR);
        stack.font_at(index, FontStyle::REGULAR).has_color_glyphs()
    };
    assert!(!umbrella_in_color([SCISSORS, UMBRELLA]));
    assert!(!umbrella_in_color([UMBRELLA, SCISSORS]));
}

#[test]
fn scalars_the_host_draws_from_one_face_share_its_id() {
    let (stack, _) = system_stack(&[("\u{2602}", SYMBOLS2), ("\u{2702}", SYMBOLS2)]);
    assert_eq!(
        stack.resolve_styled_index(UMBRELLA, FontStyle::REGULAR),
        stack.resolve_styled_index(SCISSORS, FontStyle::REGULAR)
    );
}

#[test]
fn an_emoji_presentation_scalar_asks_the_host_for_color_first() {
    let (stack, _) = system_stack(&[("\u{2615}\u{FE0F}", EMOJI), ("\u{2615}\u{FE0E}", SYMBOLS2)]);
    let index = stack.resolve_styled_index(HOT_BEVERAGE, FontStyle::REGULAR);
    assert!(stack.font_at(index, FontStyle::REGULAR).has_color_glyphs());
    let text = stack.first_cover_styled_index(HOT_BEVERAGE, FontStyle::REGULAR);
    assert!(!stack.font_at(text, FontStyle::REGULAR).has_color_glyphs());
    assert_ne!(text, 0);
}

#[test]
fn a_mono_answer_to_a_color_request_is_not_taken_as_color() {
    let (stack, _) = system_stack(&[
        ("\u{2615}\u{FE0F}", SYMBOLS2),
        ("\u{2615}\u{FE0E}", SYMBOLS2),
    ]);
    assert_eq!(
        stack.resolve_color_index(HOT_BEVERAGE, FontStyle::REGULAR),
        None
    );
    let index = stack.resolve_styled_index(HOT_BEVERAGE, FontStyle::REGULAR);
    assert_eq!(index, stack.len());
}

/// A CBDT face has no outline to draw in text presentation.
#[test]
fn a_color_answer_without_an_outline_stays_in_color_for_text() {
    let (stack, _) = system_stack(&[("\u{2602}", EMOJI)]);
    let index = stack.first_cover_styled_index(UMBRELLA, FontStyle::REGULAR);
    assert_ne!(index, 0);
    assert!(stack.font_at(index, FontStyle::REGULAR).has_color_glyphs());
}

/// The pinned set behind a host to ask, with Monaspace Argon NF
/// standing in for a symbols-only Nerd Font.
struct Hosted<P> {
    files: P,
    asked: Arc<AtomicUsize>,
}

impl<P: FaceProvider> FaceProvider for Hosted<P> {
    type Id = P::Id;

    fn find_face(&self, family: &str, weight: u16, italic: bool) -> Option<P::Id> {
        let family = if NERD_FONT_FAMILIES.contains(&family) {
            "Monaspace Argon NF"
        } else {
            family
        };
        self.files.find_face(family, weight, italic)
    }

    fn find_sibling(&self, id: P::Id, family: &str, weight: u16, italic: bool) -> Option<P::Id> {
        self.files.find_sibling(id, family, weight, italic)
    }

    fn default_monospace(&self, weight: u16) -> Result<(P::Id, String), ShapingError> {
        self.files.default_monospace(weight)
    }

    fn resolve_face(&self, id: P::Id) -> Option<ResolvedFace> {
        self.files.resolve_face(id)
    }

    fn family_of(&self, id: P::Id) -> Option<String> {
        self.files.family_of(id)
    }

    fn system_fallback(&self, _primary: P::Id) -> Option<Box<dyn SystemFallback>> {
        Some(Box::new(ScriptedSystem {
            answers: HashMap::new(),
            asked: self.asked.clone(),
            fail_first: AtomicBool::new(false),
        }))
    }
}

#[test]
fn a_nerd_glyph_draws_from_the_nerd_font_without_asking_the_host() {
    const GIT_BRANCH: char = '\u{F418}';
    let asked = Arc::new(AtomicUsize::new(0));
    let hosted = Hosted {
        files: pinned_system(),
        asked: asked.clone(),
    };
    let stack = FontStack::auto_discover_in(
        &hosted,
        Some(TEST_FONT_FAMILY),
        &[],
        &[],
        &StyleFaces::default(),
    )
    .expect("the pinned primary loads");
    let index = stack.resolve_styled_index(GIT_BRANCH, FontStyle::REGULAR);
    assert!(
        index > 0 && index < stack.len(),
        "{index} of {}",
        stack.len()
    );
    assert!(
        stack
            .font_at(index, FontStyle::REGULAR)
            .display_name()
            .starts_with("Monaspace Argon NF")
    );
    assert_eq!(asked.load(Ordering::Relaxed), 0);
}

#[test]
fn a_failed_lookup_is_asked_again() {
    let (system, asked) = scripted_system(&[("\u{2602}", SYMBOLS2)]);
    system.fail_first.store(true, Ordering::Relaxed);
    let stack = FontStack::new(Arc::new(font())).with_system_fallback(Box::new(system));
    assert_eq!(stack.resolve_styled_index(UMBRELLA, FontStyle::REGULAR), 0);
    assert_eq!(
        stack.resolve_styled_index(UMBRELLA, FontStyle::REGULAR),
        stack.len()
    );
    assert_eq!(asked.load(Ordering::Relaxed), 2);
}

#[test]
fn a_system_face_shapes_with_the_primary_features() {
    let (system, _) = scripted_system(&[("\u{2602}", SYMBOLS2)]);
    let features = vec!["calt".to_owned()];
    let stack = FontStack::with_primary_features(Arc::new(font()), features.clone())
        .with_system_fallback(Box::new(system));
    let index = stack.resolve_styled_index(UMBRELLA, FontStyle::REGULAR);
    assert_eq!(stack.features_for(index, FontStyle::REGULAR), features);
}

#[test]
fn a_full_pool_draws_notdef_for_a_face_it_does_not_hold() {
    let (system, _) = scripted_system(&[("\u{2602}", SYMBOLS2), ("\u{2702}", EMOJI)]);
    let faces = SystemFaces::new(Box::new(system), 1);
    let text = Presentation::Text;
    assert_eq!(faces.slot_for(UMBRELLA, text, FontStyle::REGULAR), Some(0));
    assert_eq!(faces.slot_for(SCISSORS, text, FontStyle::REGULAR), None);
}

/// The pinned variable face and its instance 16's position as
/// `(tag, value)` pairs.
fn semiwide_position() -> (skrifa::Font, Vec<(u32, f32)>) {
    let files = pinned_files(&["truetype/Monaspace Neon Var.ttf"]);
    let data: FontBytes = Arc::new(std::fs::read(&files[0]).expect("the variable face reads"));
    let face = metadata::model_face(&data, 0).expect("the face parses");
    let position = face
        .axes()
        .iter()
        .map(|axis| u32::from_be_bytes(axis.tag().to_be_bytes()))
        .zip(
            face.named_instances()
                .get(16)
                .expect("the instance exists")
                .user_coords(),
        )
        .collect();
    (face, position)
}

#[test]
fn a_host_position_resolves_to_the_named_instance_there() {
    let (font, mut position) = semiwide_position();
    assert_eq!(
        provider::instance_at(&font, &position),
        Some(provider::AxisPosition::Named(16))
    );
    position[0].1 += 0.25;
    assert_eq!(
        provider::instance_at(&font, &position),
        Some(provider::AxisPosition::Named(16))
    );
}

#[test]
fn a_host_position_no_instance_names_is_unrepresentable() {
    let (font, mut position) = semiwide_position();
    let wdth = u32::from_be_bytes(*b"wdth");
    let at = position
        .iter()
        .position(|&(tag, _)| tag == wdth)
        .expect("the face has wdth");
    position[at].1 += 3.0;
    assert_eq!(provider::instance_at(&font, &position), None);

    let (_, mut position) = semiwide_position();
    position.push((u32::from_be_bytes(*b"ZZZZ"), 0.0));
    assert_eq!(provider::instance_at(&font, &position), None);
}

#[test]
fn a_host_position_at_every_default_is_representable() {
    let (font, _) = semiwide_position();
    let position: Vec<(u32, f32)> = font
        .axes()
        .iter()
        .map(|axis| {
            (
                u32::from_be_bytes(axis.tag().to_be_bytes()),
                axis.default_value(),
            )
        })
        .collect();
    assert!(provider::instance_at(&font, &position).is_some());
}

/// `instances().nth(16)` of the pinned variable face is `SemiWide
/// Regular`: its `wdth` sits off the default while `wght` is 400.
#[test]
fn a_named_instance_keeps_its_width_through_regular_and_bold_views() {
    let default = pinned_variable_font();
    let name = default
        .face
        .named_instances()
        .get(16)
        .and_then(|i| metadata::english_name(&default.face, i.subfamily_name()));
    assert_eq!(name.as_deref(), Some("SemiWide Regular"));

    let semiwide = default.at_instance(16).expect("the instance exists");
    assert!(
        semiwide.regular_view(None, &[]).is_none(),
        "the regular view stays at the instance"
    );
    let width = axis_coord(&semiwide, *b"wdth");
    assert_ne!(
        width,
        F2Dot14::ZERO,
        "SemiWide must sit off the default width"
    );

    let bold = semiwide.at_style(BOLD_WEIGHT, false).expect("wght moves");
    assert_eq!(axis_coord(&bold, *b"wdth"), width);
    assert!(axis_coord(&bold, *b"wght") > axis_coord(&semiwide, *b"wght"));
    assert!(default.at_instance(10_000).is_none());
}

use super::*;

/// A mark with no precomposed glyph attaches at the face's anchors, so
/// the cluster is no wider than its base.
#[cfg(not(windows))]
#[test]
fn a_mark_without_a_precomposed_glyph_sits_on_its_base() {
    let stack = pinned_stack(&[]);
    let style = FontStyle::REGULAR;
    let mut shaper = Shaper::new();
    for text in ["q\u{301}", "x\u{323}", "g\u{308}\u{301}"] {
        let ours = shaper.shape_cluster(&stack, 32.0, style, text);
        let base = text.chars().next().expect("a base");
        let font = stack.font_at(ours.font_id, style);
        let base_advance = font.advance_px(base, 32.0).expect("the face maps the base");
        let total: f32 = ours.glyphs.iter().map(|g| g.advance_px).sum();
        assert!(
            (total - base_advance).abs() <= 0.01,
            "{text:?}: {:?}",
            ours.glyphs
        );
        assert!(
            ours.glyphs[1..]
                .iter()
                .all(|g| g.advance_px == 0.0 && g.x_offset_px != 0.0),
            "{text:?}: every mark is attached, {:?}",
            ours.glyphs
        );
    }
}

/// The sequence applies before positioning, so the mark sits on the
/// variant at no advance of its own.
#[cfg(not(windows))]
#[test]
fn a_mark_after_a_variation_sequence_keeps_the_variant() {
    let stack = pinned_stack(&[]);
    let style = FontStyle::REGULAR;
    let mut shaper = Shaper::new();
    let variant = shaper
        .shape_cluster(&stack, 18.0, style, "葛\u{E0100}")
        .glyphs[0];
    let default = shaper.shape_cluster(&stack, 18.0, style, "葛").glyphs[0];
    assert_ne!(variant.glyph_id, default.glyph_id);
    let marked = shaper.shape_cluster(&stack, 18.0, style, "葛\u{E0100}\u{301}");
    assert_eq!(marked.glyphs.len(), 2, "{:?}", marked.glyphs);
    assert_eq!(marked.glyphs[0].glyph_id, variant.glyph_id);
    assert!(marked.glyphs[1].advance_px == 0.0, "{:?}", marked.glyphs);
}

/// A mark the face does not map shapes as `.notdef` at zero advance,
/// as `HarfBuzz` zeroes every mark, so the cluster stays one cell wide.
#[cfg(not(windows))]
#[test]
fn a_mark_the_face_does_not_map_shapes_as_a_zero_advance_notdef() {
    let probe = load_pinned(TRAK_PROBE);
    assert!(probe.has_glyph('e') && !probe.has_glyph('\u{301}'));
    let stack = FontStack::new(probe);
    let style = FontStyle::REGULAR;
    let ours = Shaper::new().shape_cluster(&stack, 18.0, style, "e\u{301}");
    assert_eq!(ours.glyphs.len(), 2, "{:?}", ours.glyphs);
    assert_eq!(ours.glyphs[1].glyph_id, 0);
    assert!(ours.glyphs[1].advance_px == 0.0, "{:?}", ours.glyphs);
    let base = stack.font_at(0, style).advance_px('e', 18.0).expect("an e");
    assert!((ours.glyphs[0].advance_px - base).abs() <= 0.01);
}

/// A `CBDT` face's empty `glyf` entries are no outline; a text face's
/// glyphs are.
#[cfg(not(windows))]
#[test]
fn only_an_inked_outline_counts_as_one() {
    let emoji = load_pinned("NotoColorEmoji.ttf");
    let mut mapped = 0;
    for (cp, _) in emoji.face.charmap().unicodes() {
        let Some(c) = char::from_u32(cp) else {
            continue;
        };
        assert!(!emoji.has_outline(c), "U+{cp:04X}");
        mapped += 1;
    }
    assert!(mapped > 1000, "{mapped}");
    let text = Font::load_test_font();
    assert!(('!'..='~').all(|c| text.has_outline(c)));
    assert!(!text.has_outline(' '));
}

/// Unicode's `Default_Ignorable_Code_Point`, the set
/// `BufferFlags::REMOVE_DEFAULT_IGNORABLES` deletes from.
const fn is_default_ignorable(c: char) -> bool {
    matches!(
        c,
        '\u{AD}'
            | '\u{34F}'
            | '\u{61C}'
            | '\u{115F}'..='\u{1160}'
            | '\u{17B4}'..='\u{17B5}'
            | '\u{180B}'..='\u{180F}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{3164}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
            | '\u{FFA0}'
            | '\u{FFF0}'..='\u{FFF8}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0000}'..='\u{E0FFF}'
    )
}

#[cfg(not(windows))]
fn span_faces() -> &'static [Font; 2] {
    static FACES: OnceLock<[Font; 2]> = OnceLock::new();
    FACES.get_or_init(|| {
        let emoji = pinned_files(&[EMOJI]);
        let mut db = FileSet::default();
        db.add_file(&emoji[0]).expect("the emoji face reads");
        [
            font(),
            Font::load_face_id(&db, 0).expect("the emoji face loads"),
        ]
    })
}

fn arb_cluster_text() -> impl Strategy<Value = String> {
    let interesting = prop::sample::select(vec![
        'a',
        'm',
        'i',
        '-',
        '>',
        '=',
        '#',
        '1',
        '\u{20E3}',
        '\u{FE0E}',
        '\u{FE0F}',
        '\u{200D}',
        '\u{200B}',
        '\u{301}',
        '\u{308}',
        '\u{2764}',
        '\u{2B50}',
        '\u{1F525}',
        '\u{1F1EF}',
        '\u{1F1F5}',
        '\u{1F3FD}',
        '\u{845B}',
        '\u{E0100}',
    ]);
    proptest::collection::vec(prop_oneof![any::<char>(), interesting], 0..10)
        .prop_map(String::from_iter)
}

#[cfg(not(windows))]
proptest! {
    #[test]
    fn shaped_spans_cover_the_text_in_order(text in arb_cluster_text()) {
        for font in span_faces() {
            let out = Shaper::new().shape_run(font, 16.0, &[], &text);
            if out.is_empty() {
                prop_assert!(
                    text.chars().all(is_default_ignorable),
                    "only default-ignorables shape to nothing: {:?}",
                    text
                );
                continue;
            }
            let len = text.len();
            for pair in out.windows(2) {
                prop_assert!(pair[0].source_byte_start <= pair[1].source_byte_start);
            }
            for g in &out {
                let start = g.source_byte_start as usize;
                let end = start + g.source_byte_len as usize;
                prop_assert!(start < end && end <= len, "{:?} in {:?}", g, text);
                prop_assert!(text.is_char_boundary(start) && text.is_char_boundary(end));
            }
            prop_assert_eq!(out[0].source_byte_start, 0);
            let last = out[out.len() - 1];
            prop_assert_eq!((last.source_byte_start + last.source_byte_len) as usize, len);
        }
    }
}

/// The renderer falls back to the per-char path for a cluster that
/// shapes to nothing.
#[cfg(not(windows))]
#[test]
fn default_ignorables_alone_shape_to_no_glyphs() {
    let font = font();
    let mut shaper = Shaper::new();
    for text in ["\u{FE0F}", "\u{200D}", "\u{FE0F}\u{200B}"] {
        assert_eq!(shaper.shape_run(&font, 16.0, &[], text), [], "{text:?}");
    }
}

/// Without `REMOVE_DEFAULT_IGNORABLES` the face's unmapped VS15
/// would shape to a second, blank glyph.
#[cfg(not(windows))]
#[test]
fn an_unmapped_selector_adds_no_glyph() {
    let symbols = load_pinned(SYMBOLS2);
    assert!(!symbols.has_glyph('\u{FE0E}'));
    let out = Shaper::new().shape_run(&symbols, 18.0, &[], "\u{2B50}\u{FE0E}");
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].source_byte_len, 6);
}

/// `BEGINNING_OF_TEXT` would insert U+25CC before a mark with no base.
#[cfg(not(windows))]
#[test]
fn a_lone_mark_shapes_without_a_dotted_circle() {
    let font = font();
    let mark = font.glyph_for('\u{301}').expect("the face maps U+0301");
    let out = Shaper::new().shape_run(&font, 16.0, &[], "\u{301}");
    let ids: Vec<GlyphId> = out.iter().map(|g| g.glyph_id).collect();
    assert_eq!(ids, [mark]);
}

/// Shape plans are cached per face and feature list, so a shaper
/// that has served other pairs answers as a fresh one does.
#[cfg(not(windows))]
#[test]
fn a_reused_shaper_matches_a_fresh_one_across_faces_and_features() {
    let [mono, emoji] = span_faces();
    let feature_sets = [Vec::new(), vec!["-calt".to_owned(), "ss03".to_owned()]];
    let mut shared = Shaper::new();
    let mut differs = false;
    for _ in 0..3 {
        for (font, text) in [(mono, "mim->"), (emoji, "#\u{20E3}\u{1F1EF}\u{1F1F5}")] {
            let outputs: Vec<Vec<ShapedGlyph>> = feature_sets
                .iter()
                .map(|features| {
                    let fresh = Shaper::new().shape_run(font, 16.0, features, text);
                    assert_eq!(
                        shared.shape_run(font, 16.0, features, text),
                        fresh,
                        "{text:?} {features:?}"
                    );
                    fresh
                })
                .collect();
            differs |= outputs[0] != outputs[1];
        }
    }
    assert!(differs, "the feature sets shape differently");
}

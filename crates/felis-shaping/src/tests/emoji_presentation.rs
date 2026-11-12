use super::*;

/// A base plus combining mark shapes without panicking. The GPOS
/// offset is font-dependent, so no portable offset assertion exists.
#[test]
fn combining_mark_cluster_shapes_without_panicking() {
    let stack = stack();
    let mut shaper = Shaper::new();
    let shaping = shaper.shape_cluster(&stack, 32.0, FontStyle::REGULAR, "a\u{0301}");
    assert!(
        !shaping.glyphs.is_empty(),
        "a mark cluster must shape to at least the base glyph",
    );
}

/// ❤️‍🔥 shapes against the color-emoji face, not the mono face a bare
/// U+2764 resolves to, or GSUB never forms the ligature.
#[test]
fn emoji_zwj_with_text_default_base_shapes_against_emoji_face() {
    let stack = stack();
    let heart_face = stack.resolve_styled_index('\u{2764}', FontStyle::REGULAR);
    let emoji_face = stack.resolve_styled_index('\u{1F525}', FontStyle::REGULAR);
    assert_ne!(
        heart_face, emoji_face,
        "the pinned set keeps U+2764 off the emoji face"
    );
    let mut shaper = Shaper::new();
    let cluster = shaper.shape_cluster(
        &stack,
        32.0,
        FontStyle::REGULAR,
        "\u{2764}\u{FE0F}\u{200D}\u{1F525}",
    );
    assert_eq!(
        cluster.font_id, emoji_face,
        "heart-on-fire must resolve to the emoji face, not the mono heart face"
    );
}

/// A bare ❤️ (VS16) resolves to the color face; the same base without
/// VS16 keeps its mono face.
#[test]
fn standalone_vs16_symbol_prefers_the_color_face() {
    let stack = stack();
    let plain = stack.resolve_styled_index('\u{2764}', FontStyle::REGULAR);
    let color_face = stack
        .resolve_color_index('\u{2764}', FontStyle::REGULAR)
        .expect("the pinned color face covers U+2764");
    assert_ne!(
        plain, color_face,
        "the pinned set resolves a bare U+2764 to a mono face"
    );
    let mut shaper = Shaper::new();
    let with_vs16 = shaper.shape_cluster(&stack, 32.0, FontStyle::REGULAR, "\u{2764}\u{FE0F}");
    assert_eq!(
        with_vs16.font_id, color_face,
        "❤️ (VS16) must resolve to the color face"
    );
    let no_vs16 = shaper.shape_cluster(&stack, 32.0, FontStyle::REGULAR, "\u{2764}");
    assert_eq!(
        no_vs16.font_id, plain,
        "a bare ❤ without VS16 must stay on its text-presentation face"
    );
}

const EMOJI_DEFAULT: [char; 8] = ['⭐', '⚡', '☕', '⌚', '⛔', '☔', '♿', '🀄'];

/// The symbol faces sit before the emoji face, so a mono face covers
/// some of these first; each still resolves to the color face.
#[test]
fn an_emoji_presentation_scalar_resolves_to_the_color_face() {
    let stack = pinned_stack(&[]);
    let style = FontStyle::REGULAR;
    let overridden = EMOJI_DEFAULT
        .into_iter()
        .filter(|&c| {
            !stack
                .font_at(stack.first_cover_styled_index(c, style), style)
                .has_color_glyphs()
        })
        .count();
    assert!(
        overridden > 0,
        "no mono face in the pinned set covers one of them"
    );
    for c in EMOJI_DEFAULT {
        assert!(stack.resolve(c, style).has_color_glyphs(), "{c:?}");
        assert!(
            stack
                .font_at(stack.resolve_styled_index(c, style), style)
                .has_color_glyphs(),
            "{c:?}"
        );
    }
}

/// Text-default symbols keep the first face covering them, which the
/// chain's symbol-before-emoji order makes a mono one.
#[test]
fn a_text_presentation_symbol_keeps_its_first_covering_face() {
    let stack = pinned_stack(&[]);
    let style = FontStyle::REGULAR;
    for c in ['⏸', '✳', '❤', '☺', '⏭'] {
        assert_eq!(
            stack.resolve_styled_index(c, style),
            stack.first_cover_styled_index(c, style),
            "{c:?}"
        );
    }
}

/// An explicit `font.fallback` list orders everything but the default
/// emoji: a mono face listed first does not take `⭐`, and with no
/// color face listed the first covering face still draws it.
#[test]
fn an_explicit_fallback_order_yields_to_emoji_presentation() {
    let style = FontStyle::REGULAR;
    let with_emoji = pinned_stack(&[
        "Noto Sans Symbols 2",
        "Noto Sans Symbols",
        "Noto Color Emoji",
    ]);
    let mono = with_emoji.first_cover_styled_index('⭐', style);
    assert!(
        !with_emoji.font_at(mono, style).has_color_glyphs(),
        "a mono face lists ⭐ first"
    );
    assert!(with_emoji.resolve('⭐', style).has_color_glyphs());
    let mono_only = pinned_stack(&["Noto Sans Symbols 2"]);
    assert_eq!(mono_only.first_cover_styled_index('⭐', style), mono);
    assert!(
        mono_only.resolve('⭐', style).has_color_glyphs(),
        "the discovered color face still draws the default emoji"
    );
}

/// The declared entries come first, and discovery still covers what
/// they miss without adding a declared face a second time.
#[test]
fn an_explicit_fallback_comes_before_discovery_without_repeating() {
    let style = FontStyle::REGULAR;
    let auto = pinned_stack(&[]);
    let stack = pinned_stack(&["Noto Color Emoji"]);
    let names = stack.describe().fallbacks;
    assert!(names[0].starts_with("Noto Color Emoji"));
    assert_eq!(names.iter().filter(|n| **n == names[0]).count(), 1);
    assert_eq!(names.len(), auto.describe().fallbacks.len());
    let han = stack.resolve('\u{845B}', style);
    assert!(
        han.has_glyph('\u{845B}'),
        "discovery covers what the declared face misses"
    );
}

/// A VS15 asks for the text face, a VS16 for the color face.
#[test]
fn a_variation_selector_picks_the_star_face() {
    let stack = pinned_stack(&[]);
    let style = FontStyle::REGULAR;
    let mut shaper = Shaper::new();
    let text = shaper.shape_cluster(&stack, 18.0, style, "\u{2B50}\u{FE0E}");
    assert_eq!(text.font_id, stack.first_cover_styled_index('⭐', style));
    assert!(!stack.font_at(text.font_id, style).has_color_glyphs());
    let emoji = shaper.shape_cluster(&stack, 18.0, style, "\u{2B50}\u{FE0F}");
    assert!(stack.font_at(emoji.font_id, style).has_color_glyphs());
}

/// The astral anchor would otherwise put `🀄︎` on the color face; the
/// VS15 right after the base wins over it.
#[test]
fn a_vs15_keeps_an_astral_emoji_on_its_text_face() {
    let stack = pinned_stack(&[]);
    let style = FontStyle::REGULAR;
    let mono = stack.first_cover_styled_index('🀄', style);
    assert!(
        !stack.font_at(mono, style).has_color_glyphs(),
        "a mono face covers 🀄 first"
    );
    let mut shaper = Shaper::new();
    let text = shaper.shape_cluster(&stack, 18.0, style, "\u{1F004}\u{FE0E}");
    assert_eq!(text.font_id, mono);
    let emoji = shaper.shape_cluster(&stack, 18.0, style, "\u{1F004}\u{FE0F}");
    assert!(stack.font_at(emoji.font_id, style).has_color_glyphs());
}

/// The keycap ligature needs `#` and U+20E3 adjacent: the VS16
/// between them must not split it into three glyphs.
#[test]
fn a_keycap_with_vs16_shapes_to_the_one_keycap_glyph() {
    let stack = pinned_stack(&[]);
    let style = FontStyle::REGULAR;
    let mut shaper = Shaper::new();
    for base in ['#', '1', '*'] {
        let cluster = shaper.shape_cluster(&stack, 18.0, style, &format!("{base}\u{FE0F}\u{20E3}"));
        let face = stack.font_at(cluster.font_id, style);
        assert!(
            face.has_color_glyphs(),
            "{base}: the keycap resolves to the color face"
        );
        let bare = shaper.shape_run(
            face,
            18.0,
            stack.features_for(cluster.font_id, style),
            &format!("{base}\u{20E3}"),
        );
        assert_eq!(bare.len(), 1, "{base}: the face ligates the bare keycap");
        let ids: Vec<_> = cluster.glyphs.iter().map(|g| g.glyph_id).collect();
        assert_eq!(
            ids,
            [bare[0].glyph_id],
            "{base}: the VS16 keycap draws the same glyph"
        );
    }
}

/// Noto Sans CJK JP names a variant for `葛` + U+E0100, keeps the
/// default for U+E0101, and has no entry for U+E0102.
#[test]
fn an_ideographic_variation_sequence_shapes_to_the_face_variant() {
    let stack = pinned_stack(&[]);
    let style = FontStyle::REGULAR;
    let mut shaper = Shaper::new();
    let plain = shaper.shape_cluster(&stack, 18.0, style, "葛");
    assert_ne!(plain.font_id, 0, "the CJK fallback covers 葛");
    let glyph_ids = |cluster: &ClusterShaping| -> Vec<GlyphId> {
        cluster.glyphs.iter().map(|g| g.glyph_id).collect()
    };
    let cases = [
        ("a variant entry", '\u{E0100}', false),
        ("a default entry", '\u{E0101}', true),
        ("no entry", '\u{E0102}', true),
    ];
    for (case, selector, keeps_default) in cases {
        let sequence = shaper.shape_cluster(&stack, 18.0, style, &format!("葛{selector}"));
        assert_eq!(sequence.font_id, plain.font_id, "{case}");
        assert_eq!(sequence.glyphs.len(), 1, "{case}");
        assert_eq!(
            glyph_ids(&sequence) == glyph_ids(&plain),
            keeps_default,
            "{case}"
        );
    }
    let variant = shaper.shape_cluster(&stack, 18.0, style, "葛\u{E0100}");
    let marked = shaper.shape_cluster(&stack, 18.0, style, "葛\u{E0100}\u{0301}");
    assert_eq!(
        marked.glyphs.first().map(|g| g.glyph_id),
        variant.glyphs.first().map(|g| g.glyph_id),
        "a mark after the sequence leaves the base on the variant"
    );
}

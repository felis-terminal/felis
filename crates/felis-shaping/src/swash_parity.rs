//! Holds harfrust's shaping to the swash shaping it replaced, workarounds
//! included, for as long as swash stays a dependency.

use std::borrow::Cow;

use skrifa::raw::{
    FontData, FontRead,
    tables::cmap::{Cmap, CmapSubtable, MapVariant},
};
use swash::{
    Setting,
    shape::{Direction, ShapeContext},
};

use std::sync::Arc;

use crate::{
    ClusterShaping, Font, FontStack, FontStyle, GlyphId, ShapedGlyph, Shaper, StyleFaces,
    cluster_face, parse_feature, provider::files::FileSet, test_font_dir,
};

struct Reference {
    glyphs: Vec<ShapedGlyph>,
    ligatures: Vec<bool>,
}

fn swash_shape_run(
    ctx: &mut ShapeContext,
    font: &Font,
    px: f32,
    features: &[String],
    text: &str,
) -> Reference {
    let mut reference = Reference {
        glyphs: Vec::new(),
        ligatures: Vec::new(),
    };
    if text.is_empty() {
        return reference;
    }
    let settings: Vec<Setting<u16>> = features
        .iter()
        .filter_map(|s| parse_feature(s))
        .map(|f| Setting {
            tag: u32::from_be_bytes(f.tag.to_be_bytes()),
            value: u16::try_from(f.value).expect("the grammar keeps values in u16"),
        })
        .collect();
    let mut shaper = ctx
        .builder(font.font_ref())
        .size(px)
        .direction(Direction::LeftToRight)
        .features(settings.iter().copied())
        .normalized_coords(&font.coords)
        .build();
    shaper.add_str(text);
    shaper.shape_with(|cluster| {
        for g in cluster.glyphs {
            reference.glyphs.push(ShapedGlyph {
                glyph_id: GlyphId::from(g.id),
                source_byte_start: cluster.source.start,
                source_byte_len: cluster.source.end.saturating_sub(cluster.source.start),
                advance_px: g.advance,
                x_offset_px: g.x,
                y_offset_px: g.y,
            });
            reference.ligatures.push(cluster.is_ligature());
        }
    });
    reference
}

fn swash_shape_cluster(
    ctx: &mut ShapeContext,
    stack: &FontStack,
    px: f32,
    style: FontStyle,
    text: &str,
) -> ClusterShaping {
    let font_id = cluster_face(stack, style, text).expect("a non-empty cluster");
    let font = stack.font_at(font_id, style);
    let shaped_text = without_unmapped_selectors(text, |c| font.has_glyph(c));
    let mut glyphs = swash_shape_run(
        ctx,
        font,
        px,
        stack.features_for(font_id, style),
        &shaped_text,
    )
    .glyphs;
    substitute_variant(font, px, text, &mut glyphs);
    ClusterShaping { font_id, glyphs }
}

const fn is_variation_selector(c: char) -> bool {
    matches!(c, '\u{FE00}'..='\u{FE0F}' | '\u{E0100}'..='\u{E01EF}')
}

fn without_unmapped_selectors(text: &str, maps: impl Fn(char) -> bool) -> Cow<'_, str> {
    let dropped = |i: usize, c: char| i > 0 && is_variation_selector(c) && !maps(c);
    if !text.char_indices().any(|(i, c)| dropped(i, c)) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        text.char_indices()
            .filter(|&(i, c)| !dropped(i, c))
            .map(|(_, c)| c)
            .collect(),
    )
}

fn variant_glyph(font: &Font, base: char, selector: char) -> Option<GlyphId> {
    let table = font.font_ref().table(swash::tag_from_bytes(b"cmap"))?;
    let cmap = Cmap::read(FontData::new(table)).ok()?;
    let variants = cmap.encoding_records().iter().find_map(|record| {
        match record.subtable(cmap.offset_data()).ok()? {
            CmapSubtable::Format14(variants) => Some(variants),
            _ => None,
        }
    })?;
    match variants.map_variant(base, selector)? {
        MapVariant::Variant(glyph) => Some(glyph.to_u32()),
        MapVariant::UseDefault => None,
    }
}

fn substitute_variant(font: &Font, px: f32, text: &str, glyphs: &mut [ShapedGlyph]) {
    let mut chars = text.chars();
    let (Some(base), Some(selector)) = (chars.next(), chars.next()) else {
        return;
    };
    if !is_variation_selector(selector) {
        return;
    }
    let face = font.font_ref();
    let nominal = face.charmap().map(base);
    let [glyph] = glyphs else {
        return;
    };
    if glyph.glyph_id != GlyphId::from(nominal) {
        return;
    }
    let Some(variant) = variant_glyph(font, base, selector) else {
        return;
    };
    let Ok(narrow) = u16::try_from(variant) else {
        return;
    };
    let metrics = face.glyph_metrics(&font.coords).scale(px);
    glyph.advance_px += metrics.advance_width(narrow) - metrics.advance_width(nominal);
    glyph.glyph_id = variant;
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() <= 0.01
}

fn same_glyph(ours: &ShapedGlyph, theirs: &ShapedGlyph, spans: bool) -> bool {
    ours.glyph_id == theirs.glyph_id
        && (!spans
            || (ours.source_byte_start == theirs.source_byte_start
                && ours.source_byte_len == theirs.source_byte_len))
        && close(ours.advance_px, theirs.advance_px)
        && close(ours.x_offset_px, theirs.x_offset_px)
        && close(ours.y_offset_px, theirs.y_offset_px)
}

fn same_glyphs(ours: &[ShapedGlyph], theirs: &[ShapedGlyph], spans: bool) -> bool {
    ours.len() == theirs.len()
        && ours
            .iter()
            .zip(theirs)
            .all(|(a, b)| same_glyph(a, b, spans))
}

fn load(name: &str) -> Font {
    let mut db = FileSet::default();
    db.add_file(test_font_dir().join(name))
        .expect("the pinned face reads");
    Font::load_face_id(&db, 0).expect("the pinned face loads")
}

fn pinned_stack() -> FontStack {
    let mut db = FileSet::default();
    db.add_dir(test_font_dir());
    FontStack::auto_discover_in(&db, None, &[], &[], &StyleFaces::default())
        .expect("the pinned set loads")
}

const ROW: &str = "user@host:~/src/felis$ cargo test -p felis-shaping -- --nocapture # => != ->";

const OPERATORS: [&str; 25] = [
    "->", "=>", "!=", "==", "===", "<=", ">=", "<-", "<->", "::", "//", "/*", "&&", "||", "www",
    "mim", "fi", "ffi", "|>", "<|", "-->", "<!--", "...", "#{", "0x1F",
];

const FEATURE_SETS: [&[&str]; 6] = [
    &[],
    &["-calt"],
    &["calt", "liga"],
    &[
        "ss01", "ss02", "ss03", "ss04", "ss05", "ss06", "ss07", "ss08", "ss09",
    ],
    &["ss03"],
    &["-calt", "liga 0", "dlig"],
];

const SIZES: [f32; 5] = [10.0, 13.0, 16.0, 21.5, 40.0];

#[test]
fn runs_shape_as_swash_shaped_them() {
    let regular = load("opentype/MonaspaceNeon-Regular.otf");
    let bold = load("truetype/Monaspace Neon Var.ttf")
        .at_style(700, false)
        .expect("the variable face moves to wght 700");
    let ascii: String = (0x20u8..0x7f).map(char::from).collect();
    let texts: Vec<&str> = [ascii.as_str(), ROW].into_iter().chain(OPERATORS).collect();
    let mut ctx = ShapeContext::new();
    let mut shaper = Shaper::new();
    let mut mismatches = Vec::new();
    let mut cases = 0;
    for (face, font) in [("regular", &regular), ("wght 700", &bold)] {
        for set in FEATURE_SETS {
            let features: Vec<String> = set.iter().map(|&f| f.to_owned()).collect();
            for px in SIZES {
                for &text in &texts {
                    cases += 1;
                    let theirs = swash_shape_run(&mut ctx, font, px, &features, text);
                    for (glyph, ligature) in theirs.glyphs.iter().zip(&theirs.ligatures) {
                        let start = glyph.source_byte_start as usize;
                        let span = &text[start..start + glyph.source_byte_len as usize];
                        assert_eq!(
                            *ligature,
                            span.chars().count() > 1,
                            "{face} {set:?} {px} {text:?}: swash's ligature flag is the span"
                        );
                    }
                    let ours = shaper.shape_run(font, px, &features, text);
                    if !same_glyphs(&ours, &theirs.glyphs, true) {
                        mismatches.push(format!(
                            "{face} {set:?} {px} {text:?}\n  harfrust {ours:?}\n  swash    {:?}",
                            theirs.glyphs
                        ));
                    }
                }
            }
        }
    }
    assert!(cases > 1000, "the corpus ran: {cases}");
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

const SAME_CLUSTERS: [&str; 19] = [
    "#\u{FE0F}\u{20E3}",
    "1\u{FE0F}\u{20E3}",
    "*\u{FE0F}\u{20E3}",
    "#\u{20E3}",
    "\u{2764}\u{FE0F}\u{200D}\u{1F525}",
    "\u{2764}\u{FE0F}",
    "\u{2B50}\u{FE0F}",
    "\u{1F004}\u{FE0F}",
    "\u{2B50}\u{FE0E}",
    "\u{1F004}\u{FE0E}",
    "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
    "\u{1F44D}\u{1F3FD}",
    "\u{1F1EF}\u{1F1F5}",
    "葛\u{E0100}",
    "葛\u{E0101}",
    "葛\u{E0102}",
    "\u{E9}",
    "\u{F1}",
    "e\u{301}",
];

/// Spans are left out: swash's were over the selector-stripped text,
/// and the cluster path reads none.
#[test]
fn clusters_shape_as_swash_shaped_them() {
    let stack = pinned_stack();
    let style = FontStyle::REGULAR;
    let mut ctx = ShapeContext::new();
    let mut shaper = Shaper::new();
    for text in SAME_CLUSTERS {
        let ours = shaper.shape_cluster(&stack, 18.0, style, text);
        let theirs = swash_shape_cluster(&mut ctx, &stack, 18.0, style, text);
        assert_eq!(ours.font_id, theirs.font_id, "{text:?}");
        assert!(
            same_glyphs(&ours.glyphs, &theirs.glyphs, false),
            "{text:?}\n  harfrust {:?}\n  swash    {:?}",
            ours.glyphs,
            theirs.glyphs
        );
    }
}

/// swash left these marks unpositioned at their full advance; harfrust
/// attaches them at the face's anchors, so the cluster is no wider than
/// its base.
#[test]
fn a_mark_without_a_precomposed_glyph_sits_on_its_base() {
    let stack = pinned_stack();
    let style = FontStyle::REGULAR;
    let mut ctx = ShapeContext::new();
    let mut shaper = Shaper::new();
    for text in ["q\u{301}", "x\u{323}", "g\u{308}\u{301}"] {
        let ours = shaper.shape_cluster(&stack, 32.0, style, text);
        let theirs = swash_shape_cluster(&mut ctx, &stack, 32.0, style, text);
        assert!(
            !same_glyphs(&ours.glyphs, &theirs.glyphs, false),
            "{text:?} is no longer a difference"
        );
        let base = text.chars().next().expect("a base");
        let font = stack.font_at(ours.font_id, style);
        let base_advance = font.advance_px(base, 32.0).expect("the face maps the base");
        let total: f32 = ours.glyphs.iter().map(|g| g.advance_px).sum();
        assert!(close(total, base_advance), "{text:?}: {:?}", ours.glyphs);
        assert!(
            ours.glyphs[1..]
                .iter()
                .all(|g| g.advance_px == 0.0 && g.x_offset_px != 0.0),
            "{text:?}: every mark is attached, {:?}",
            ours.glyphs
        );
    }
}

/// swash's variant substitution stopped at a cluster with a mark; harfrust
/// applies the sequence before positioning, so the mark sits on the variant.
#[test]
fn a_mark_after_a_variation_sequence_keeps_the_variant() {
    let stack = pinned_stack();
    let style = FontStyle::REGULAR;
    let mut ctx = ShapeContext::new();
    let mut shaper = Shaper::new();
    let variant = shaper
        .shape_cluster(&stack, 18.0, style, "葛\u{E0100}")
        .glyphs[0];
    let default = shaper.shape_cluster(&stack, 18.0, style, "葛").glyphs[0];
    assert_ne!(variant.glyph_id, default.glyph_id);
    let ours = shaper.shape_cluster(&stack, 18.0, style, "葛\u{E0100}\u{301}");
    let theirs = swash_shape_cluster(&mut ctx, &stack, 18.0, style, "葛\u{E0100}\u{301}");
    assert_eq!(theirs.glyphs[0].glyph_id, default.glyph_id);
    assert_eq!(ours.glyphs[0].glyph_id, variant.glyph_id);
    assert_eq!(ours.glyphs.len(), 2, "{:?}", ours.glyphs);
    assert!(ours.glyphs[1].advance_px == 0.0, "{:?}", ours.glyphs);
}

/// swash gave a `.notdef` mark its full advance, so the cluster drew two
/// cells wide; harfrust zeroes it, as `HarfBuzz` does for every mark.
#[test]
fn a_mark_the_face_does_not_map_shapes_as_a_zero_advance_notdef() {
    let probe = load(crate::tests::TRAK_PROBE);
    assert!(probe.has_glyph('e') && !probe.has_glyph('\u{301}'));
    let stack = FontStack::new(Arc::new(probe));
    let style = FontStyle::REGULAR;
    let mut ctx = ShapeContext::new();
    let ours = Shaper::new().shape_cluster(&stack, 18.0, style, "e\u{301}");
    let theirs = swash_shape_cluster(&mut ctx, &stack, 18.0, style, "e\u{301}");
    assert!(
        !same_glyphs(&ours.glyphs, &theirs.glyphs, false),
        "no longer a difference: {:?}",
        ours.glyphs
    );
    assert_eq!(theirs.glyphs.len(), 2);
    assert!(theirs.glyphs[1].advance_px > 0.0, "{:?}", theirs.glyphs);
    assert_eq!(ours.glyphs.len(), 2, "{:?}", ours.glyphs);
    assert_eq!(ours.glyphs[1].glyph_id, 0);
    assert!(ours.glyphs[1].advance_px == 0.0, "{:?}", ours.glyphs);
    let base = stack.font_at(0, style).advance_px('e', 18.0).expect("an e");
    assert!(close(ours.glyphs[0].advance_px, base));
}

//! Cell metrics and face metadata against swash's reading of the same
//! faces.

use skrifa::{
    GlyphSourceOrder, Tag,
    raw::{FontRef, types::NameId},
};
use swash::{Stretch, StringId, Style};

use crate::{
    CellMetrics, Font, goldens,
    metadata::{self, Attributes},
};

/// The cell felis builds around swash's line box.
fn swash_cell(font: &Font, px: f32) -> CellMetrics {
    let metrics = font.swash_ref().metrics(&font.swash_coords()).scale(px);
    let width = font.advance_px('M', px).unwrap_or(px * 0.6);
    CellMetrics::from_scaled(width, metrics.ascent, metrics.descent, metrics.leading)
}

/// Equal in their f32 bits at every size, and so the golden, which is
/// generated from swash's line box here and from felis in the golden
/// test, holds both to the same cells.
#[test]
fn line_metrics_match_swash_bit_for_bit() {
    let sizes = goldens::px_grid();
    let mut differ = Vec::new();
    for (name, bytes, index) in goldens::every_face() {
        let font = goldens::load(&bytes, index);
        for (position, view) in goldens::positions(&font) {
            let theirs = view.swash_ref().metrics(&view.swash_coords());
            for &px in &sizes {
                let scaled = theirs.scale(px);
                let ours = view.line_metrics(px);
                let bits = |line: (f32, f32, f32)| <[f32; 3]>::from(line).map(f32::to_bits);
                if bits(ours) != bits((scaled.ascent, scaled.descent, scaled.leading)) {
                    differ.push(format!("{name} {position} {px}: {ours:?} vs {scaled:?}"));
                }
            }
        }
    }
    assert!(
        differ.is_empty(),
        "{} differ, first {:?}",
        differ.len(),
        differ.first()
    );
    goldens::assert_golden("cell_metrics.txt", &goldens::cell_metrics(swash_cell));
}

fn swash_display(font: swash::FontRef<'_>) -> String {
    let strings = font.localized_strings();
    [StringId::Full, StringId::Family]
        .into_iter()
        .find_map(|id| {
            strings
                .find_by_id(id, Some("en"))
                .or_else(|| strings.find_by_id(id, None))
        })
        .map_or_else(|| "unnamed".to_owned(), |found| found.to_string())
}

fn swash_families(font: swash::FontRef<'_>) -> Vec<String> {
    let strings = font.localized_strings();
    let typographic = strings
        .clone()
        .any(|name| name.id() == StringId::TypographicFamily);
    let id = if typographic {
        StringId::TypographicFamily
    } else {
        StringId::Family
    };
    let mut names: Vec<(bool, String)> = strings
        .filter(|name| name.id() == id)
        .map(|name| (!name.language().starts_with("en"), name.to_string()))
        .filter(|(_, name)| !name.is_empty())
        .collect();
    names.sort_by_key(|(foreign, _)| *foreign);
    let mut unique = Vec::new();
    for (_, name) in names {
        if !unique.contains(&name) {
            unique.push(name);
        }
    }
    unique
}

/// Everything a provider or the stack reads from a face's tables, equal
/// on every face; the two accepted differences, instance PostScript names
/// and the normalization of a fractional axis value, are asserted in their
/// new form.
#[test]
fn face_metadata_matches_swash() {
    let mut differ = Vec::new();
    let mut check = |label: String, equal: bool| {
        if !equal {
            differ.push(label);
        }
    };
    for (name, bytes, index) in goldens::every_face() {
        let data = bytes.as_ref().as_ref();
        let theirs = swash::FontRef::from_index(data, index as usize).expect("swash parses it");
        let face = metadata::model_face(&bytes, index).expect("the model parses it");
        let font = goldens::load(&bytes, index);

        check(
            format!("{name} display"),
            font.display_name() == swash_display(theirs),
        );
        check(
            format!("{name} families"),
            metadata::family_names(&face) == swash_families(theirs),
        );
        let mut every: Vec<String> = theirs
            .localized_strings()
            .filter(|n| matches!(n.id(), StringId::Family | StringId::TypographicFamily))
            .map(|n| n.to_string())
            .filter(|n| !n.is_empty())
            .collect();
        let mut ours = metadata::all_family_names(&face);
        every.sort();
        ours.sort();
        check(format!("{name} every family"), ours == every);
        check(
            format!("{name} postscript"),
            metadata::any_name(&face, NameId::POSTSCRIPT_NAME).unwrap_or_default()
                == theirs
                    .localized_strings()
                    .find_by_id(StringId::PostScript, None)
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
        );
        let attributes = theirs.attributes();
        check(
            format!("{name} attributes"),
            metadata::attributes(data, index)
                == Attributes {
                    weight: attributes.weight().0,
                    italic: attributes.style() != Style::Normal,
                    off_width: attributes.stretch().raw().abs_diff(Stretch::NORMAL.raw()),
                },
        );
        check(
            format!("{name} color"),
            font.has_color_glyphs()
                == (theirs.color_palettes().len() != 0 || theirs.color_strikes().len() != 0),
        );
        check(
            format!("{name} color sources"),
            font.has_color_glyphs()
                == (!face.palettes().is_empty()
                    || !face
                        .bitmap_strikes_with(GlyphSourceOrder::COLOR_BITMAPS)
                        .is_empty()),
        );

        let (charmap, swash_charmap) = (face.charmap(), theirs.charmap());
        let unmapped = (0..=0x0010_FFFF_u32)
            .filter(|cp| !(0xD800..0xE000).contains(cp))
            .filter(|&cp| {
                charmap.map_unicode(cp).map_or(0, skrifa::GlyphId::to_u32)
                    != u32::from(swash_charmap.map(cp))
            })
            .count();
        check(format!("{name} charmap: {unmapped} scalars"), unmapped == 0);
        let theirs_cmap = theirs.table(swash::tag_from_bytes(b"cmap"));
        let ours_cmap = FontRef::from_index(data, index).ok().and_then(|f| {
            f.table_data(Tag::new(b"cmap"))
                .map(|t| t.as_bytes().as_ptr())
        });
        check(
            format!("{name} cmap slice"),
            ours_cmap == theirs_cmap.map(<[u8]>::as_ptr),
        );

        let axes: Vec<(u32, f32, f32, f32)> = face
            .axes()
            .iter()
            .map(|a| {
                let tag = u32::from_be_bytes(a.tag().to_be_bytes());
                (tag, a.min_value(), a.default_value(), a.max_value())
            })
            .collect();
        let swash_axes: Vec<(u32, f32, f32, f32)> = theirs
            .variations()
            .map(|v| (v.tag(), v.min_value(), v.default_value(), v.max_value()))
            .collect();
        check(format!("{name} axes"), axes == swash_axes);
        for (at, instance) in theirs.instances().enumerate() {
            let ours = face.named_instances().get(at as u32).expect("the instance");
            check(
                format!("{name} instance {at} values"),
                ours.user_coords().collect::<Vec<_>>() == instance.values().collect::<Vec<_>>(),
            );
            check(
                format!("{name} instance {at} name"),
                metadata::english_name(&face, ours.subfamily_name())
                    == instance
                        .name(Some("en"))
                        .or_else(|| instance.name(None))
                        .map(|n| n.to_string()),
            );
            let record = ours.postscript_name();
            check(
                format!("{name} instance {at} PostScript name is the fvar record's"),
                record.and_then(|id| metadata::any_name(&face, id))
                    == record.and_then(|id| {
                        theirs
                            .localized_strings()
                            .find_by_id(StringId::from_raw(id.to_u16()), None)
                            .map(|s| s.to_string())
                    }),
            );
        }
        for (tag, value) in axis_settings(&face) {
            let ours = metadata::normalized_coords(data, index, &[(tag, value)]);
            let swash_coords: Vec<i16> = theirs
                .variations()
                .normalized_coords([(u32::from_be_bytes(tag.to_be_bytes()), value)])
                .collect();
            let within_a_unit = ours.len() == swash_coords.len()
                && ours
                    .iter()
                    .zip(&swash_coords)
                    .all(|(a, b)| a.to_bits().abs_diff(*b) <= 1);
            check(format!("{name} {tag}={value} normalized"), within_a_unit);
        }
    }
    assert!(differ.is_empty(), "{} differ: {differ:#?}", differ.len());
}

fn axis_settings(face: &skrifa::Font) -> Vec<(Tag, f32)> {
    face.axes()
        .iter()
        .flat_map(|axis| {
            let (min, max) = (axis.min_value(), axis.max_value());
            (0..=37).map(move |step| (axis.tag(), min + (max - min) * step as f32 / 37.0))
        })
        .collect()
}

/// Only an outline counts; a `CBDT` face's empty `glyf` entries do not.
#[test]
fn has_outline_matches_swash_on_every_mapped_emoji() {
    let font = crate::test_support::pinned("NotoColorEmoji.ttf");
    let theirs = font.swash_ref();
    let mut ctx = swash::scale::ScaleContext::new();
    let mut scaler = ctx.builder(theirs).build();
    let mut checked = 0;
    for (cp, glyph) in font.face.charmap().unicodes() {
        let Some(c) = char::from_u32(cp) else {
            continue;
        };
        let swash = scaler
            .scale_outline(u16::try_from(glyph.to_u32()).expect("a 16-bit glyph"))
            .is_some_and(|outline| !outline.is_empty());
        assert_eq!(font.has_outline(c), swash, "U+{cp:04X}");
        checked += 1;
    }
    assert!(checked > 1000, "{checked}");
}

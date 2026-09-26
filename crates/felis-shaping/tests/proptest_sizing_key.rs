//! `SizingKey` packs six OSC 66 sizing fields, a style and a fallback id
//! into one `u32`: each field reads back what was written, a `with_*`
//! setter disturbs no other field, and distinct tuples never share a slot.

use felis_shaping::{FontStyle, SizingKey};
use proptest::prelude::*;

/// `(scale, cell_width, frac_num, frac_den, valign, halign)`.
type Fields = (u8, u8, u8, u8, u8, u8);

/// The six sizing fields as `SizingKey::new` masks them.
const fn masked(fields: Fields) -> Fields {
    (
        fields.0 & 0xF,
        fields.1 & 0xF,
        fields.2 & 0xF,
        fields.3 & 0xF,
        fields.4 & 0xF,
        fields.5 & 0xF,
    )
}

const fn key(fields: Fields) -> SizingKey {
    SizingKey::new(fields.0, fields.1, fields.2, fields.3, fields.4, fields.5)
}

const fn style(bold: bool, italic: bool) -> FontStyle {
    FontStyle { bold, italic }
}

proptest! {
    #[test]
    fn a_fresh_key_reads_back_the_fields_it_was_built_from(
        fields in any::<Fields>(),
    ) {
        let (scale, _, frac_num, frac_den, _, _) = masked(fields);
        let k = key(fields);
        prop_assert_eq!(k.scale(), scale);
        prop_assert_eq!(k.frac_num(), frac_num);
        prop_assert_eq!(k.frac_den(), frac_den);
        prop_assert_eq!(k.style(), FontStyle::REGULAR);
        prop_assert_eq!(k.font_id(), 0);
    }

    /// Two cells that differ in any sizing field must not share an atlas
    /// slot, including the three fields no accessor exposes.
    #[test]
    fn keys_collide_only_when_every_masked_field_agrees(
        left in any::<Fields>(),
        right in any::<Fields>(),
    ) {
        prop_assert_eq!(key(left) == key(right), masked(left) == masked(right));
    }

    #[test]
    fn a_style_round_trips_and_leaves_every_other_field_alone(
        fields in any::<Fields>(),
        (bold, italic) in any::<(bool, bool)>(),
        font_id in any::<usize>(),
    ) {
        let base = key(fields).with_font_id(font_id);
        let keyed = base.with_style(style(bold, italic));
        prop_assert_eq!(keyed.style(), style(bold, italic));
        prop_assert_eq!(keyed.font_id(), base.font_id());
        prop_assert_eq!(keyed.scale(), base.scale());
        prop_assert_eq!(keyed.frac_num(), base.frac_num());
        prop_assert_eq!(keyed.frac_den(), base.frac_den());
        prop_assert_eq!(keyed.with_style(FontStyle::REGULAR), base);
    }

    /// Ids past 63 saturate into one bucket rather than wrapping into a
    /// neighbouring face's slot.
    #[test]
    fn a_font_id_round_trips_up_to_the_saturation_bucket(
        fields in any::<Fields>(),
        (bold, italic) in any::<(bool, bool)>(),
        font_id in prop_oneof![0..=0x3F_usize, 0x40..=usize::MAX],
    ) {
        let base = key(fields).with_style(style(bold, italic));
        let keyed = base.with_font_id(font_id);
        prop_assert_eq!(keyed.font_id(), font_id.min(0x3F));
        prop_assert_eq!(keyed.style(), base.style());
        prop_assert_eq!(keyed.scale(), base.scale());
        prop_assert_eq!(keyed.frac_num(), base.frac_num());
        prop_assert_eq!(keyed.frac_den(), base.frac_den());
        prop_assert_eq!(keyed.with_font_id(0), base);
    }
}

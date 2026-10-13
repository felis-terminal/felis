//! Hash / Eq consistency for the hand-packed `Attributes` hash.
//!
//! `pack` is private, so pens are compared through `Hash` with the
//! deterministic `DefaultHasher`, where a collision between distinct
//! pens can only come from the pack itself.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::hash::{DefaultHasher, Hash, Hasher};

use felis_grid::{AttrFlags, Attributes, Color, UnderlineStyle};
use proptest::prelude::*;

fn hash_of(pen: &Attributes) -> u64 {
    let mut h = DefaultHasher::new();
    pen.hash(&mut h);
    h.finish()
}

fn assert_hash_matches_eq(pens: impl IntoIterator<Item = Attributes>) {
    let mut by_hash: HashMap<u64, Attributes> = HashMap::new();
    for pen in pens {
        match by_hash.entry(hash_of(&pen)) {
            Entry::Occupied(seen) => {
                assert_eq!(*seen.get(), pen, "distinct pens must not share a hash");
            }
            Entry::Vacant(slot) => {
                slot.insert(pen);
            }
        }
    }
}

#[test]
fn finite_colors_hash_injectively_across_all_color_slots() {
    let colors = std::iter::once(Color::Default).chain((0..=255).map(Color::Indexed));
    assert_hash_matches_eq(colors.flat_map(|color| {
        (0..3).map(move |slot| {
            let mut pen = Attributes::default();
            match slot {
                0 => pen.fg = color,
                1 => pen.bg = color,
                _ => pen.underline_color = color,
            }
            pen
        })
    }));
}

fn arb_color() -> impl Strategy<Value = Color> {
    prop_oneof![
        Just(Color::Default),
        any::<u8>().prop_map(Color::Indexed),
        any::<(u8, u8, u8)>().prop_map(|(r, g, b)| Color::Rgb(r, g, b)),
    ]
}

fn arb_attributes() -> impl Strategy<Value = Attributes> {
    (
        arb_color(),
        arb_color(),
        arb_color(),
        any::<u16>(),
        arb_underline_style(),
    )
        .prop_map(
            |(fg, bg, underline_color, bits, underline_style)| Attributes {
                fg,
                bg,
                underline_color,
                flags: AttrFlags::from_bits_truncate(bits),
                underline_style,
            },
        )
}

fn arb_underline_style() -> impl Strategy<Value = UnderlineStyle> {
    prop_oneof![
        Just(UnderlineStyle::Single),
        Just(UnderlineStyle::Double),
        Just(UnderlineStyle::Curly),
        Just(UnderlineStyle::Dotted),
        Just(UnderlineStyle::Dashed),
    ]
}

proptest! {
    #[test]
    fn rgb_pens_one_bit_apart_hash_differently(
        rgb in any::<[u8; 3]>(),
        channel in 0usize..3,
        bit in 0u8..8,
    ) {
        let mut flipped = rgb;
        flipped[channel] ^= 1 << bit;
        let pen = |[r, g, b]: [u8; 3]| Attributes { fg: Color::Rgb(r, g, b), ..Attributes::default() };
        prop_assert_eq!(hash_of(&pen(rgb)), hash_of(&pen(rgb)));
        prop_assert_ne!(hash_of(&pen(rgb)), hash_of(&pen(flipped)));
    }

    #[test]
    fn attributes_hash_equal_until_one_field_differs(
        a in arb_attributes(),
        field in 0usize..5,
        color in arb_color(),
        flag_bit in 0u32..16,
        style in arb_underline_style(),
    ) {
        let mut b = a;
        prop_assert_eq!(hash_of(&a), hash_of(&b));
        match field {
            0 => b.fg = color,
            1 => b.bg = color,
            2 => b.underline_color = color,
            3 => b.flags ^= AttrFlags::from_bits_truncate(1 << flag_bit),
            _ => b.underline_style = style,
        }
        prop_assume!(a != b);
        prop_assert_ne!(hash_of(&a), hash_of(&b));
    }
}

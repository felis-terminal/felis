use crate::test_support::drive;
use crate::*;
use felis_vt::Parser;

use super::*;

const NO_SUB: u32 = 0;

/// Bit `i` ⇒ slot `i` was opened by `:`, as the parser reports it.
fn sub_at(idxs: &[u32]) -> u32 {
    idxs.iter().fold(0u32, |m, &b| m | (1u32 << b))
}

#[test]
fn malformed_extended_drops_the_discriminator_and_keeps_going() {
    let mut a = Attributes::default();
    a.apply_sgr(&[38], NO_SUB);
    assert_eq!(a.fg, Color::Default);

    let mut a = Attributes::default();
    a.apply_sgr(&[38, 1], NO_SUB);
    assert!(a.flags.contains(AttrFlags::BOLD));
}

#[test]
fn reset_after_extended_returns_to_default() {
    let mut a = Attributes::default();
    a.apply_sgr(&[38, 2, 9, 9, 9, 0], NO_SUB);
    assert_eq!(a.fg, Color::Default);
    assert_eq!(a.bg, Color::Default);
    assert!(a.flags.is_empty());
    assert_eq!(a.underline_color, Color::Default);
    assert_eq!(a.underline_style, UnderlineStyle::Single);
}

#[test]
fn sgr_4_no_subparam_is_single_underline() {
    let mut a = Attributes::default();
    a.apply_sgr(&[4], NO_SUB);
    assert!(a.flags.contains(AttrFlags::UNDERLINE));
    assert_eq!(a.underline_style, UnderlineStyle::Single);
}

#[test]
fn sgr_4_followed_by_semicolon_3_is_underline_plus_italic() {
    // Without sub-param awareness `\e[4;3m` would collapse to "curly
    // underline".
    let mut a = Attributes::default();
    a.apply_sgr(&[4, 3], NO_SUB);
    assert!(a.flags.contains(AttrFlags::UNDERLINE));
    assert!(a.flags.contains(AttrFlags::ITALIC));
    assert_eq!(a.underline_style, UnderlineStyle::Single);
}

#[test]
fn sgr_4_colon_2_is_double_underline() {
    let mut a = Attributes::default();
    a.apply_sgr(&[4, 2], sub_at(&[1]));
    assert!(a.flags.contains(AttrFlags::UNDERLINE));
    assert_eq!(a.underline_style, UnderlineStyle::Double);
}

#[test]
fn sgr_4_colon_3_is_curly_underline() {
    let mut a = Attributes::default();
    a.apply_sgr(&[4, 3], sub_at(&[1]));
    assert!(a.flags.contains(AttrFlags::UNDERLINE));
    assert_eq!(a.underline_style, UnderlineStyle::Curly);
    assert!(!a.flags.contains(AttrFlags::ITALIC));
}

#[test]
fn sgr_4_colon_4_is_dotted_and_5_is_dashed() {
    let mut a = Attributes::default();
    a.apply_sgr(&[4, 4], sub_at(&[1]));
    assert_eq!(a.underline_style, UnderlineStyle::Dotted);
    let mut a = Attributes::default();
    a.apply_sgr(&[4, 5], sub_at(&[1]));
    assert_eq!(a.underline_style, UnderlineStyle::Dashed);
}

#[test]
fn sgr_4_colon_0_clears_underline() {
    let mut a = Attributes::default();
    a.apply_sgr(&[4, 0], sub_at(&[1]));
    assert!(!a.flags.contains(AttrFlags::UNDERLINE));
    assert_eq!(a.underline_style, UnderlineStyle::Single);
}

#[test]
fn sgr_21_is_double_underline_legacy_form() {
    let mut a = Attributes::default();
    a.apply_sgr(&[21], NO_SUB);
    assert!(a.flags.contains(AttrFlags::UNDERLINE));
    assert_eq!(a.underline_style, UnderlineStyle::Double);
}

#[test]
fn sgr_24_clears_underline_and_resets_style() {
    let mut a = Attributes::default();
    a.apply_sgr(&[4, 3], sub_at(&[1]));
    assert_eq!(a.underline_style, UnderlineStyle::Curly);
    a.apply_sgr(&[24], NO_SUB);
    assert!(!a.flags.contains(AttrFlags::UNDERLINE));
    assert_eq!(a.underline_style, UnderlineStyle::Single);
}

#[test]
fn sgr_59_resets_underline_color() {
    let mut a = Attributes::default();
    a.apply_sgr(&[58, 5, 9], NO_SUB);
    a.apply_sgr(&[59], NO_SUB);
    assert_eq!(a.underline_color, Color::Default);
}

#[test]
fn sgr_4_followed_by_unknown_subparam_keeps_underline_on() {
    // Forward-compat: an unknown style keeps underline lit and falls
    // back to single.
    let mut a = Attributes::default();
    a.apply_sgr(&[4, 99], sub_at(&[1]));
    assert!(a.flags.contains(AttrFlags::UNDERLINE));
    assert_eq!(a.underline_style, UnderlineStyle::Single);
}

#[test]
fn sgr_bold_then_reset() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 5);
    drive(&mut p, &mut g, b"\x1b[1mA\x1b[0mB");
    assert!(
        g.style(g.cell(0, 0).unwrap().style)
            .flags
            .contains(AttrFlags::BOLD)
    );
    assert!(
        !g.style(g.cell(0, 1).unwrap().style)
            .flags
            .contains(AttrFlags::BOLD)
    );
}

#[test]
fn overflowed_sgr_is_discarded_whole() {
    // 20 params overrun `MAX_PARAMS` (16); REQ-903 discards the whole
    // sequence rather than acting on its truncated prefix.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 3);
    let mut seq = Vec::from(*b"\x1b[");
    for _ in 0..20 {
        seq.extend_from_slice(b"1;");
    }
    seq.extend_from_slice(b"31mX");
    drive(&mut p, &mut g, &seq);
    let attrs = g.style(g.cell(0, 0).unwrap().style);
    assert_eq!(attrs.fg, Color::Default);
    assert!(!attrs.flags.contains(AttrFlags::BOLD));
}

#[test]
fn sgr_16_color_fg_and_bg() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 3);
    drive(&mut p, &mut g, b"\x1b[31;44mX");
    let attrs = g.style(g.cell(0, 0).unwrap().style);
    assert_eq!(attrs.fg, Color::Indexed(1));
    assert_eq!(attrs.bg, Color::Indexed(4));
}

/// Deleting any one arm would silently drop that attribute. (BOLD and
/// the underline arms have their own tests above.)
#[test]
fn each_set_flag_sgr_lights_its_own_bit() {
    let cases = [
        (2u16, AttrFlags::FAINT),
        (3, AttrFlags::ITALIC),
        (5, AttrFlags::BLINK),
        (6, AttrFlags::BLINK),
        (7, AttrFlags::REVERSE),
        (8, AttrFlags::CONCEAL),
        (9, AttrFlags::STRIKETHROUGH),
        (53, AttrFlags::OVERLINE),
    ];
    for (code, flag) in cases {
        let mut a = Attributes::default();
        a.apply_sgr(&[code], NO_SUB);
        assert_eq!(a.flags, flag, "SGR {code} should set exactly {flag:?}");
    }
}

/// Deleting an arm would leave the bit stuck on.
#[test]
fn each_clear_flag_sgr_removes_only_its_own_bit() {
    let cases = [
        (23u16, AttrFlags::ITALIC),
        (25, AttrFlags::BLINK),
        (27, AttrFlags::REVERSE),
        (28, AttrFlags::CONCEAL),
        (29, AttrFlags::STRIKETHROUGH),
        (55, AttrFlags::OVERLINE),
    ];
    let all = AttrFlags::FAINT
        | AttrFlags::ITALIC
        | AttrFlags::BLINK
        | AttrFlags::REVERSE
        | AttrFlags::CONCEAL
        | AttrFlags::STRIKETHROUGH
        | AttrFlags::OVERLINE;
    for (code, flag) in cases {
        let mut a = Attributes {
            flags: all,
            ..Attributes::default()
        };
        a.apply_sgr(&[code], NO_SUB);
        assert!(!a.flags.contains(flag), "SGR {code} should clear {flag:?}");
        assert_eq!(
            a.flags,
            all & !flag,
            "SGR {code} cleared more than {flag:?}"
        );
    }
}

#[test]
fn sgr_22_clears_bold_and_faint_together() {
    let mut a = Attributes {
        flags: AttrFlags::BOLD | AttrFlags::FAINT | AttrFlags::ITALIC,
        ..Attributes::default()
    };
    a.apply_sgr(&[22], NO_SUB);
    assert!(!a.flags.contains(AttrFlags::BOLD));
    assert!(!a.flags.contains(AttrFlags::FAINT));
    assert!(a.flags.contains(AttrFlags::ITALIC));
}

#[test]
fn sgr_39_and_49_reset_fg_and_bg_to_default() {
    let mut a = Attributes {
        fg: Color::Indexed(1),
        bg: Color::Indexed(2),
        ..Attributes::default()
    };
    a.apply_sgr(&[39], NO_SUB);
    assert_eq!(a.fg, Color::Default);
    assert_eq!(a.bg, Color::Indexed(2), "39 must not touch bg");
    a.apply_sgr(&[49], NO_SUB);
    assert_eq!(a.bg, Color::Default);
}

/// A wrong per-arm offset (`p - 30`, `p - 90 + 8`, …) lands on the
/// wrong palette slot.
#[test]
fn four_bit_palette_arms_map_to_exact_indices() {
    for (code, idx) in (30u16..=37).zip(0u8..=7) {
        let mut a = Attributes::default();
        a.apply_sgr(&[code], NO_SUB);
        assert_eq!(a.fg, Color::Indexed(idx), "SGR {code}");
    }
    for (code, idx) in (40u16..=47).zip(0u8..=7) {
        let mut a = Attributes::default();
        a.apply_sgr(&[code], NO_SUB);
        assert_eq!(a.bg, Color::Indexed(idx), "SGR {code}");
    }
    for (code, idx) in (90u16..=97).zip(8u8..=15) {
        let mut a = Attributes::default();
        a.apply_sgr(&[code], NO_SUB);
        assert_eq!(a.fg, Color::Indexed(idx), "SGR {code}");
    }
    for (code, idx) in (100u16..=107).zip(8u8..=15) {
        let mut a = Attributes::default();
        a.apply_sgr(&[code], NO_SUB);
        assert_eq!(a.bg, Color::Indexed(idx), "SGR {code}");
    }
}

use proptest::prelude::*;

#[derive(Debug, Clone, Copy)]
enum Slot {
    Fg,
    Bg,
    Underline,
}

#[derive(Debug, Clone, Copy)]
enum Syntax {
    Classic,
    Colon,
    ColonWithColorSpace,
}

/// `[1, slot, ..payload.., 4]` and its sub-param mask: the bold before
/// and the underline after pin that the colour run consumes its own
/// slots and no more.
fn extended_run(slot: Slot, payload: &[u16], syntax: Syntax) -> (Vec<u16>, u32) {
    let code = match slot {
        Slot::Fg => 38,
        Slot::Bg => 48,
        Slot::Underline => 58,
    };
    let mut params = vec![1, code];
    params.extend_from_slice(payload);
    params.push(4);
    let mask = match syntax {
        Syntax::Classic => 0,
        Syntax::Colon | Syntax::ColonWithColorSpace => {
            (2..2 + payload.len()).fold(0u32, |m, i| m | (1u32 << i))
        }
    };
    (params, mask)
}

fn color_of(a: &Attributes, slot: Slot) -> Color {
    match slot {
        Slot::Fg => a.fg,
        Slot::Bg => a.bg,
        Slot::Underline => a.underline_color,
    }
}

fn other_slots(slot: Slot) -> [Slot; 2] {
    match slot {
        Slot::Fg => [Slot::Bg, Slot::Underline],
        Slot::Bg => [Slot::Fg, Slot::Underline],
        Slot::Underline => [Slot::Fg, Slot::Bg],
    }
}

fn arb_slot() -> impl Strategy<Value = Slot> {
    prop_oneof![Just(Slot::Fg), Just(Slot::Bg), Just(Slot::Underline)]
}

proptest! {
    /// SGR 38 / 48 / 58 carry the same two colour payloads in the same
    /// three syntaxes, and each lands on its own slot alone.
    #[test]
    fn extended_indexed_color_lands_on_its_slot(
        slot in arb_slot(),
        colon in any::<bool>(),
        idx in 0u16..=400,
    ) {
        let syntax = if colon { Syntax::Colon } else { Syntax::Classic };
        let (params, mask) = extended_run(slot, &[5, idx], syntax);
        let mut a = Attributes::default();
        a.apply_sgr(&params, mask);

        prop_assert_eq!(color_of(&a, slot), Color::Indexed(idx.min(255) as u8));
        for other in other_slots(slot) {
            prop_assert_eq!(color_of(&a, other), Color::Default);
        }
        prop_assert!(a.flags.contains(AttrFlags::BOLD));
        prop_assert!(a.flags.contains(AttrFlags::UNDERLINE));
        prop_assert_eq!(a.underline_style, UnderlineStyle::Single);
    }

    /// The colon form carries a colour-space slot the classic form has
    /// no room for; read with the wrong offset the components shift.
    #[test]
    fn extended_truecolor_lands_on_its_slot(
        slot in arb_slot(),
        syntax in prop_oneof![
            Just(Syntax::Classic),
            Just(Syntax::Colon),
            Just(Syntax::ColonWithColorSpace),
        ],
        color_space in 0u16..=9,
        r in 0u16..=400,
        g in 0u16..=400,
        b in 0u16..=400,
    ) {
        let payload = match syntax {
            Syntax::ColonWithColorSpace => vec![2, color_space, r, g, b],
            _ => vec![2, r, g, b],
        };
        let (params, mask) = extended_run(slot, &payload, syntax);
        let mut a = Attributes::default();
        a.apply_sgr(&params, mask);

        prop_assert_eq!(
            color_of(&a, slot),
            Color::Rgb(r.min(255) as u8, g.min(255) as u8, b.min(255) as u8),
        );
        for other in other_slots(slot) {
            prop_assert_eq!(color_of(&a, other), Color::Default);
        }
        prop_assert!(a.flags.contains(AttrFlags::BOLD));
        prop_assert!(a.flags.contains(AttrFlags::UNDERLINE));
    }
}

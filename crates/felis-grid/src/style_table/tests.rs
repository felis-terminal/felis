use std::collections::HashSet;

use proptest::prelude::*;

use super::{PenKey, StyleId, StyleTable};
use crate::{AttrFlags, Attributes, Color, UnderlineStyle};

fn arb_color() -> impl Strategy<Value = Color> {
    prop_oneof![
        Just(Color::Default),
        (0u8..=255).prop_map(Color::Indexed),
        (0u8..=255, 0u8..=255, 0u8..=255).prop_map(|(r, g, b)| Color::Rgb(r, g, b)),
    ]
}

fn arb_attrs() -> impl Strategy<Value = Attributes> {
    (
        arb_color(),
        arb_color(),
        arb_color(),
        0u16..=0b0111_1111_1111,
        0u8..=4,
    )
        .prop_map(|(fg, bg, uc, bits, us)| Attributes {
            fg,
            bg,
            underline_color: uc,
            flags: AttrFlags::from_bits_truncate(bits),
            underline_style: match us {
                0 => UnderlineStyle::Single,
                1 => UnderlineStyle::Double,
                2 => UnderlineStyle::Curly,
                3 => UnderlineStyle::Dotted,
                _ => UnderlineStyle::Dashed,
            },
        })
}

#[test]
fn default_pen_is_id_zero() {
    let mut t = StyleTable::new();
    assert_eq!(t.intern(Attributes::default()), StyleId::DEFAULT);
    assert_eq!(*t.resolve(StyleId::DEFAULT), Attributes::default());
    assert_eq!(t.len(), 1);
    assert!(t.is_empty());
}

#[test]
fn unknown_id_resolves_to_default() {
    let t = StyleTable::new();
    assert_eq!(*t.resolve(StyleId(9999)), Attributes::default());
}

proptest! {
    #[test]
    fn intern_dedups_and_distinguishes(a in arb_attrs(), b in arb_attrs()) {
        let mut t = StyleTable::new();
        let ia1 = t.intern(a);
        let ia2 = t.intern(a);
        prop_assert_eq!(ia1, ia2);
        let ib = t.intern(b);
        prop_assert_eq!(ia1 == ib, a == b);
        prop_assert_eq!(*t.resolve(ia1), a);
        prop_assert_eq!(*t.resolve(ib), b);
    }

    #[test]
    fn resolve_after_intern_round_trips(pens in prop::collection::vec(arb_attrs(), 0..64)) {
        let mut t = StyleTable::new();
        let ids: Vec<StyleId> = pens.iter().map(|&p| t.intern(p)).collect();
        for (id, pen) in ids.iter().zip(&pens) {
            prop_assert_eq!(t.resolve(*id), pen);
        }
        let mut distinct: HashSet<Attributes> = pens.iter().copied().collect();
        distinct.insert(Attributes::default());
        prop_assert_eq!(t.len(), distinct.len());
    }

    #[test]
    fn compact_preserves_live_values(
        pens in prop::collection::vec(arb_attrs(), 0..48),
        keep_mask in prop::collection::vec(any::<bool>(), 0..48),
    ) {
        let mut t = StyleTable::new();
        let ids: Vec<StyleId> = pens.iter().map(|&p| t.intern(p)).collect();

        let mut marks = t.mark_buffer();
        let mut used: HashSet<StyleId> = HashSet::new();
        let mut want: Vec<(StyleId, Attributes)> = Vec::new();
        for (i, id) in ids.iter().enumerate() {
            if keep_mask.get(i).copied().unwrap_or(false) {
                StyleTable::mark(&mut marks, *id);
                used.insert(*id);
                want.push((*id, pens[i]));
            }
        }

        let before_default = *t.resolve(StyleId::DEFAULT);
        let len_before = t.len();
        let remap = t.compact(&marks).unwrap_or_else(|| {
            (0..len_before).map(|i| StyleId(i as u32)).collect()
        });

        prop_assert_eq!(remap[0], StyleId::DEFAULT);
        prop_assert_eq!(*t.resolve(StyleId::DEFAULT), before_default);

        for &(old, pen) in &want {
            let new = remap[old.get() as usize];
            prop_assert_eq!(*t.resolve(new), pen);
        }

        let mut distinct: HashSet<Attributes> = used
            .iter()
            .map(|id| pens[ids.iter().position(|x| x == id).unwrap()])
            .collect();
        distinct.insert(Attributes::default());
        prop_assert_eq!(t.len(), distinct.len());

        let len_after = t.len();
        for &(old, pen) in &want {
            let new = remap[old.get() as usize];
            prop_assert_eq!(t.intern(pen), new);
        }
        prop_assert_eq!(t.len(), len_after);
    }

    #[test]
    fn has_iso_protected_tracks_the_entries_through_interns_and_compactions(
        rounds in prop::collection::vec(
            (
                prop::collection::vec(arb_attrs(), 0..16),
                prop::collection::vec(any::<bool>(), 0..24),
            ),
            1..6,
        ),
    ) {
        let mut t = StyleTable::new();
        let any_protected = |t: &StyleTable| {
            (0..t.len()).any(|i| {
                t.resolve(StyleId(i as u32))
                    .flags
                    .contains(AttrFlags::ISO_PROTECTED)
            })
        };
        for (pens, keep_mask) in rounds {
            for pen in pens {
                t.intern(pen);
                prop_assert_eq!(t.has_iso_protected(), any_protected(&t));
            }
            let mut marks = t.mark_buffer();
            for (i, keep) in keep_mask.into_iter().enumerate() {
                if keep {
                    StyleTable::mark(&mut marks, StyleId(i as u32));
                }
            }
            drop(t.compact(&marks));
            prop_assert_eq!(t.has_iso_protected(), any_protected(&t));
        }
    }

    /// Two indexed colors of 48 each give more pens than cache slots, so
    /// slots collide, and compactions renumber ids the cache still holds.
    #[test]
    fn the_recent_cache_returns_what_the_dedup_index_would(
        ops in prop::collection::vec(
            prop_oneof![
                8 => (0u8..48, 0u8..48).prop_map(|(f, b)| Some((f, b))),
                1 => Just(None),
            ],
            1..600,
        ),
        keep in prop::collection::vec(any::<bool>(), 64),
    ) {
        let mut cached = StyleTable::new();
        let mut uncached = StyleTable::new();
        for (n, op) in ops.into_iter().enumerate() {
            if let Some((f, b)) = op {
                let pen = Attributes {
                    fg: Color::Indexed(f),
                    bg: Color::Indexed(b),
                    ..Attributes::default()
                };
                let want = uncached.intern_uncached(PenKey(pen.pack()), pen);
                prop_assert_eq!(cached.intern(pen), want);
            } else {
                let mut marks = cached.mark_buffer();
                for (i, m) in marks.iter_mut().enumerate() {
                    *m = keep[(i + n) % keep.len()];
                }
                prop_assert_eq!(cached.compact(&marks), uncached.compact(&marks));
            }
        }
    }
}
